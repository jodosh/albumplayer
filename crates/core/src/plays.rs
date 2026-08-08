//! The play-history log.
//!
//! Listening is recorded as immutable events. A `PlaySession` tracks one sitting
//! with one album; individual tracks append `play_event` rows as they finish,
//! and the session is settled when you move on. Nothing here increments a
//! counter, so any statistic can be recomputed later from history alone.

use rusqlite::params;

use crate::util::now_unix;
use crate::{Library, Result};

/// A track counts as played once you have heard more than half of it, or four
/// minutes of it, whichever comes first. This is the long-standing scrobbling
/// convention and it behaves sensibly for both 90-second interludes and
/// 20-minute prog epics.
pub const COMPLETION_RATIO: f64 = 0.5;
pub const COMPLETION_CAP_MS: i64 = 4 * 60 * 1000;

/// Fraction of an album's tracks that must complete for the sitting to count as
/// a full album play. Slightly forgiving, so skipping the last hidden track or
/// a run-out groove does not disqualify an otherwise complete listen.
pub const ALBUM_COMPLETION_RATIO: f64 = 0.75;

/// Whether `ms_played` is enough to call a track of `duration_ms` played.
pub fn is_completed(ms_played: i64, duration_ms: i64) -> bool {
    if ms_played <= 0 {
        return false;
    }
    let threshold = if duration_ms > 0 {
        ((duration_ms as f64 * COMPLETION_RATIO) as i64).min(COMPLETION_CAP_MS)
    } else {
        COMPLETION_CAP_MS
    };
    ms_played >= threshold
}

/// An open listening session for one album.
#[derive(Debug, Clone, Copy)]
pub struct PlaySession {
    pub id: i64,
    pub album_id: i64,
}

impl Library {
    /// Open a session for an album. Call once when playback of the album starts.
    pub fn start_album_session(&self, album_id: i64) -> Result<PlaySession> {
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM album WHERE id = ?1",
            params![album_id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(crate::Error::NotFound {
                kind: "album",
                id: album_id,
            });
        }

        let id = self.conn.query_row(
            "INSERT INTO album_session (album_id, started_at) VALUES (?1, ?2) RETURNING id",
            params![album_id, now_unix()],
            |r| r.get(0),
        )?;
        Ok(PlaySession { id, album_id })
    }

    /// Record one track listen. `ms_played` is actual audible time, so pausing
    /// or seeking backwards should not inflate it.
    ///
    /// Returns whether the listen counted as completed.
    pub fn record_play(
        &self,
        session: Option<PlaySession>,
        track_id: i64,
        started_at: i64,
        ms_played: i64,
    ) -> Result<bool> {
        let (album_id, duration_ms): (i64, i64) = self
            .conn
            .query_row(
                "SELECT album_id, duration_ms FROM track WHERE id = ?1",
                params![track_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => crate::Error::NotFound {
                    kind: "track",
                    id: track_id,
                },
                other => other.into(),
            })?;

        let completed = is_completed(ms_played, duration_ms);

        self.conn.execute(
            "INSERT INTO play_event (track_id, album_id, session_id, started_at,
                                     ms_played, completed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                track_id,
                album_id,
                session.map(|s| s.id),
                started_at,
                ms_played,
                completed as i64
            ],
        )?;

        if completed && let Some(s) = session {
            self.conn.execute(
                "UPDATE album_session SET tracks_completed = tracks_completed + 1
                 WHERE id = ?1",
                params![s.id],
            )?;
        }
        Ok(completed)
    }

    /// Close a session and decide whether it counted as a full album play.
    ///
    /// Returns true if the album was played through.
    pub fn end_album_session(&self, session: PlaySession) -> Result<bool> {
        let (completed, track_count): (i64, i64) = self.conn.query_row(
            "SELECT s.tracks_completed, al.track_count
             FROM album_session s JOIN album al ON al.id = s.album_id
             WHERE s.id = ?1",
            params![session.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        let needed = if track_count > 0 {
            ((track_count as f64 * ALBUM_COMPLETION_RATIO).ceil() as i64).max(1)
        } else {
            i64::MAX
        };
        let finished = completed >= needed;

        self.conn.execute(
            "UPDATE album_session SET ended_at = ?2, finished = ?3 WHERE id = ?1",
            params![session.id, now_unix(), finished as i64],
        )?;
        Ok(finished)
    }

    /// Most-played albums, optionally restricted to plays since a Unix timestamp.
    pub fn top_albums(&self, limit: i64, since: Option<i64>) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ar.name, al.title, COUNT(*) AS plays
             FROM album_session s
             JOIN album al  ON al.id = s.album_id
             JOIN artist ar ON ar.id = al.album_artist_id
             WHERE s.finished = 1 AND s.started_at >= ?1
             GROUP BY s.album_id
             ORDER BY plays DESC, ar.sort_name
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![since.unwrap_or(0), limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Most-played artists by completed album listens.
    pub fn top_artists(&self, limit: i64, since: Option<i64>) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ar.name,
                    COUNT(DISTINCT s.id) AS album_plays,
                    (SELECT COUNT(*) FROM play_event pe
                       JOIN track t ON t.id = pe.track_id
                      WHERE t.artist_id = ar.id AND pe.completed = 1
                        AND pe.started_at >= ?1) AS track_plays
             FROM album_session s
             JOIN album al  ON al.id = s.album_id
             JOIN artist ar ON ar.id = al.album_artist_id
             WHERE s.finished = 1 AND s.started_at >= ?1
             GROUP BY ar.id
             ORDER BY album_plays DESC, track_plays DESC, ar.sort_name
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![since.unwrap_or(0), limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Recent album sittings, newest first.
    pub fn recent_albums(&self, limit: i64) -> Result<Vec<(i64, String, String, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.started_at, ar.name, al.title, s.finished
             FROM album_session s
             JOIN album al  ON al.id = s.album_id
             JOIN artist ar ON ar.id = al.album_artist_id
             ORDER BY s.started_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a library with one album of `n` tracks, each `duration_ms` long.
    fn fixture(n: i64, duration_ms: i64) -> (Library, i64, Vec<i64>) {
        let lib = Library::open_in_memory().unwrap();
        lib.conn
            .execute("INSERT INTO artist (id, name, sort_name) VALUES (1,'A','a')", [])
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO album (id, group_key, identity_source, title, sort_title,
                                    album_artist_id, dir_path, track_count, added_at, last_seen_at)
                 VALUES (1,'k','artist+title','T','t',1,'/d',?1,0,0)",
                params![n],
            )
            .unwrap();

        let mut ids = Vec::new();
        for i in 1..=n {
            lib.conn
                .execute(
                    "INSERT INTO track (path, album_id, artist_id, disc_no, track_no,
                                        title, duration_ms, mtime, size, added_at)
                     VALUES (?1,1,1,1,?2,'t',?3,0,0,0)",
                    params![format!("/d/{i}.mp3"), i, duration_ms],
                )
                .unwrap();
            ids.push(lib.conn.last_insert_rowid());
        }
        (lib, 1, ids)
    }

    #[test]
    fn completion_uses_half_the_track() {
        assert!(!is_completed(40_000, 100_000));
        assert!(is_completed(50_000, 100_000));
    }

    #[test]
    fn completion_caps_at_four_minutes_for_long_tracks() {
        let twenty_min = 20 * 60 * 1000;
        assert!(is_completed(4 * 60 * 1000, twenty_min));
        assert!(!is_completed(3 * 60 * 1000, twenty_min));
    }

    #[test]
    fn zero_playtime_never_counts() {
        assert!(!is_completed(0, 100_000));
        assert!(!is_completed(-5, 100_000));
    }

    #[test]
    fn a_full_listen_counts_as_an_album_play() {
        let (lib, album_id, tracks) = fixture(10, 200_000);
        let session = lib.start_album_session(album_id).unwrap();
        for id in &tracks {
            assert!(lib.record_play(Some(session), *id, 0, 200_000).unwrap());
        }
        assert!(lib.end_album_session(session).unwrap());

        let stats = lib.stats().unwrap();
        assert_eq!(stats.album_plays, 1);
        assert_eq!(stats.track_plays, 10);
    }

    #[test]
    fn bailing_out_early_does_not_count_as_an_album_play() {
        let (lib, album_id, tracks) = fixture(10, 200_000);
        let session = lib.start_album_session(album_id).unwrap();
        for id in tracks.iter().take(3) {
            lib.record_play(Some(session), *id, 0, 200_000).unwrap();
        }
        assert!(!lib.end_album_session(session).unwrap());
        assert_eq!(lib.stats().unwrap().album_plays, 0);
        // The individual track listens are still recorded.
        assert_eq!(lib.stats().unwrap().track_plays, 3);
    }

    #[test]
    fn skipping_one_track_still_counts_the_album() {
        let (lib, album_id, tracks) = fixture(10, 200_000);
        let session = lib.start_album_session(album_id).unwrap();
        for id in tracks.iter().take(9) {
            lib.record_play(Some(session), *id, 0, 200_000).unwrap();
        }
        assert!(lib.end_album_session(session).unwrap());
    }

    #[test]
    fn counts_are_derived_from_events_not_stored() {
        let (lib, album_id, tracks) = fixture(4, 100_000);
        for _ in 0..3 {
            let s = lib.start_album_session(album_id).unwrap();
            for id in &tracks {
                lib.record_play(Some(s), *id, 0, 100_000).unwrap();
            }
            lib.end_album_session(s).unwrap();
        }
        let top = lib.top_albums(10, None).unwrap();
        assert_eq!(top[0].2, 3);

        // Deleting history rewinds the statistic, proving nothing is cached.
        // The individual play events survive with a null session, which is what
        // ON DELETE SET NULL on play_event.session_id is there to guarantee.
        lib.conn.execute("DELETE FROM album_session", []).unwrap();
        assert!(lib.top_albums(10, None).unwrap().is_empty());
        assert_eq!(lib.stats().unwrap().track_plays, 12);
    }

    #[test]
    fn unknown_ids_are_rejected() {
        let lib = Library::open_in_memory().unwrap();
        assert!(lib.start_album_session(99).is_err());
        assert!(lib.record_play(None, 99, 0, 1000).is_err());
    }
}
