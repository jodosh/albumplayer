//! Bridge from playback events to the library's play history.
//!
//! The engine reports what happened; the library records it. Keeping the two
//! apart means the engine never touches SQLite on its audio thread, and the
//! same bridge serves the CLI, the desktop UI, and the eventual homelab server.
//!
//! Sessions follow albums, which is the whole point: one `album_session` per
//! sitting with a record, and a `play_event` per track heard within it.

use albumplayer_core::Library;
use albumplayer_core::plays::PlaySession;
use albumplayer_core::util::now_unix;

use crate::PlayerEvent;

/// Consumes [`PlayerEvent`]s and writes them to the play log.
pub struct PlayLogger<'a> {
    library: &'a Library,
    session: Option<PlaySession>,
    /// When the track now playing started, so partial listens are timestamped
    /// from when they began rather than when they were abandoned.
    track_started_at: i64,
    /// Non-fatal logging failures, surfaced rather than silently swallowed —
    /// losing history should be visible, but must never stop the music.
    errors: Vec<String>,
}

impl<'a> PlayLogger<'a> {
    pub fn new(library: &'a Library) -> Self {
        Self {
            library,
            session: None,
            track_started_at: now_unix(),
            errors: Vec::new(),
        }
    }

    /// Feed one event in. Never fails: playback outranks bookkeeping.
    pub fn handle(&mut self, event: &PlayerEvent) {
        match event {
            PlayerEvent::AlbumStarted { album_id, .. } => {
                self.close_session();
                match self.library.start_album_session(*album_id) {
                    Ok(session) => self.session = Some(session),
                    Err(e) => self.errors.push(format!("starting session: {e}")),
                }
            }
            PlayerEvent::TrackStarted { .. } => {
                self.track_started_at = now_unix();
            }
            PlayerEvent::TrackFinished {
                track_id,
                ms_played,
                ..
            } => {
                // The library decides whether this counts as a listen; the
                // engine only reports how much was actually audible.
                if let Err(e) =
                    self.library
                        .record_play(self.session, *track_id, self.track_started_at, *ms_played)
                {
                    self.errors.push(format!("recording play: {e}"));
                }
            }
            PlayerEvent::QueueFinished => {
                self.close_session();
            }
            _ => {}
        }
    }

    /// End the current sitting. Returns true if it counted as a full album play.
    pub fn close_session(&mut self) -> bool {
        let Some(session) = self.session.take() else {
            return false;
        };
        match self.library.end_album_session(session) {
            Ok(finished) => finished,
            Err(e) => {
                self.errors.push(format!("ending session: {e}"));
                false
            }
        }
    }

    /// Problems encountered while writing history, if any.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }
}

impl Drop for PlayLogger<'_> {
    fn drop(&mut self) {
        // A session left open by an abrupt exit still gets settled.
        self.close_session();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// A library holding one album of `n` tracks.
    fn fixture(n: i64) -> (Library, Vec<i64>) {
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
                     VALUES (?1,1,1,1,?2,'t',200000,0,0,0)",
                    params![format!("/d/{i}.mp3"), i],
                )
                .unwrap();
            ids.push(lib.conn.last_insert_rowid());
        }
        (lib, ids)
    }

    fn started(album_id: i64) -> PlayerEvent {
        PlayerEvent::AlbumStarted {
            album_id,
            title: "T".into(),
        }
    }

    fn track_started(track_id: i64) -> PlayerEvent {
        PlayerEvent::TrackStarted {
            album_id: 1,
            track_id,
            title: "t".into(),
        }
    }

    fn track_finished(track_id: i64, ms_played: i64) -> PlayerEvent {
        PlayerEvent::TrackFinished {
            album_id: 1,
            track_id,
            ms_played,
        }
    }

    #[test]
    fn a_full_listen_is_recorded_as_an_album_play() {
        let (lib, tracks) = fixture(4);
        let mut logger = PlayLogger::new(&lib);

        logger.handle(&started(1));
        for id in &tracks {
            logger.handle(&track_started(*id));
            logger.handle(&track_finished(*id, 200_000));
        }
        logger.handle(&PlayerEvent::QueueFinished);

        let stats = lib.stats().unwrap();
        assert_eq!(stats.album_plays, 1);
        assert_eq!(stats.track_plays, 4);
        assert!(logger.errors().is_empty(), "{:?}", logger.errors());
    }

    #[test]
    fn skipping_through_an_album_does_not_count_it() {
        let (lib, tracks) = fixture(10);
        let mut logger = PlayLogger::new(&lib);

        logger.handle(&started(1));
        for id in tracks.iter().take(2) {
            logger.handle(&track_started(*id));
            logger.handle(&track_finished(*id, 200_000));
        }
        // Barely touched the rest.
        for id in tracks.iter().skip(2) {
            logger.handle(&track_started(*id));
            logger.handle(&track_finished(*id, 500));
        }
        logger.handle(&PlayerEvent::QueueFinished);

        let stats = lib.stats().unwrap();
        assert_eq!(stats.album_plays, 0, "not a full listen");
        assert_eq!(stats.track_plays, 2, "only the two that were really heard");
    }

    #[test]
    fn moving_to_another_album_settles_the_previous_session() {
        let (lib, tracks) = fixture(2);
        let mut logger = PlayLogger::new(&lib);

        logger.handle(&started(1));
        for id in &tracks {
            logger.handle(&track_started(*id));
            logger.handle(&track_finished(*id, 200_000));
        }
        // A second AlbumStarted arrives without an explicit end.
        logger.handle(&started(1));

        assert_eq!(lib.stats().unwrap().album_plays, 1, "the first sitting closed");
    }

    #[test]
    fn an_abrupt_exit_still_settles_the_session() {
        let (lib, tracks) = fixture(2);
        {
            let mut logger = PlayLogger::new(&lib);
            logger.handle(&started(1));
            for id in &tracks {
                logger.handle(&track_started(*id));
                logger.handle(&track_finished(*id, 200_000));
            }
            // Dropped without QueueFinished, as on Ctrl-C.
        }
        assert_eq!(lib.stats().unwrap().album_plays, 1);
    }

    #[test]
    fn logging_failures_are_reported_but_never_fatal() {
        let (lib, _) = fixture(1);
        let mut logger = PlayLogger::new(&lib);

        logger.handle(&started(999)); // no such album
        logger.handle(&track_finished(999, 1000)); // no such track

        assert_eq!(logger.errors().len(), 2, "{:?}", logger.errors());
        assert_eq!(lib.stats().unwrap().track_plays, 0);
    }
}
