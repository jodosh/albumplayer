//! Read-side queries. Every play statistic is derived from the event log here,
//! never read from a stored counter.

use std::path::PathBuf;

use rusqlite::params;

use crate::model::{AlbumRow, ArtistRow, LibraryStats, TrackRow};
use crate::{Library, Result};

/// Where an album's cover image lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumArt {
    /// An image sitting in the album's own folder, as an absolute path.
    Folder(PathBuf),
    /// A cover we fetched, named relative to the caller's art cache directory.
    /// Relative on purpose: the cache is at a different absolute path on a host
    /// than inside a container, and the database has to survive the move.
    Cached(String),
}

impl AlbumArt {
    /// Resolve to a real path, given where the cover cache lives.
    pub fn resolve(&self, cache_dir: &std::path::Path) -> PathBuf {
        match self {
            Self::Folder(path) => path.clone(),
            Self::Cached(name) => cache_dir.join(name),
        }
    }
}

/// Ordering options for album listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumSort {
    Artist,
    Title,
    Year,
    Plays,
    Added,
    LastPlayed,
}

impl AlbumSort {
    fn order_by(self) -> &'static str {
        match self {
            Self::Artist => "ar.sort_name, al.year, al.sort_title",
            Self::Title => "al.sort_title",
            Self::Year => "al.year DESC NULLS LAST, ar.sort_name",
            Self::Plays => "play_count DESC, ar.sort_name, al.sort_title",
            Self::Added => "al.added_at DESC, al.sort_title",
            Self::LastPlayed => "last_played DESC NULLS LAST, al.sort_title",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "artist" => Some(Self::Artist),
            "title" => Some(Self::Title),
            "year" => Some(Self::Year),
            "plays" => Some(Self::Plays),
            "added" => Some(Self::Added),
            "last" | "last-played" => Some(Self::LastPlayed),
            _ => None,
        }
    }
}

/// Filter applied to album and artist listings.
#[derive(Debug, Clone, Default)]
pub struct AlbumFilter {
    /// Substring match against album title or album artist.
    pub search: Option<String>,
    /// Include albums whose files are no longer on disk.
    pub include_missing: bool,
    pub limit: Option<i64>,
}

impl Library {
    pub fn albums(&self, sort: AlbumSort, filter: &AlbumFilter) -> Result<Vec<AlbumRow>> {
        let mut sql = String::from(
            "SELECT al.id, al.title, ar.name, al.year, al.track_count, al.disc_count,
                    al.duration_ms, al.is_compilation, al.dir_path, al.source_dirs,
                    COALESCE(pc.play_count, 0) AS play_count,
                    pc.last_played AS last_played
             FROM album al
             JOIN artist ar ON ar.id = al.album_artist_id
             LEFT JOIN album_play_counts pc ON pc.album_id = al.id
             WHERE 1 = 1",
        );
        if !filter.include_missing {
            sql.push_str(" AND al.present = 1");
        }
        if filter.search.is_some() {
            sql.push_str(" AND (al.title LIKE ?1 ESCAPE '\\' OR ar.name LIKE ?1 ESCAPE '\\')");
        }
        sql.push_str(" ORDER BY ");
        sql.push_str(sort.order_by());
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let map = |r: &rusqlite::Row<'_>| {
            Ok(AlbumRow {
                id: r.get(0)?,
                title: r.get(1)?,
                album_artist: r.get(2)?,
                year: r.get::<_, Option<i64>>(3)?.map(|y| y as i32),
                track_count: r.get(4)?,
                disc_count: r.get(5)?,
                duration_ms: r.get(6)?,
                is_compilation: r.get::<_, i64>(7)? != 0,
                dir_path: r.get(8)?,
                source_dirs: r.get(9)?,
                play_count: r.get(10)?,
                last_played: r.get(11)?,
            })
        };

        let rows = match &filter.search {
            Some(term) => stmt
                .query_map(params![like_pattern(term)], map)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            None => stmt
                .query_map([], map)?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    pub fn artists(&self, limit: Option<i64>) -> Result<Vec<ArtistRow>> {
        let mut sql = String::from(
            "SELECT ar.id, ar.name,
                    (SELECT COUNT(*) FROM album al
                      WHERE al.album_artist_id = ar.id AND al.present = 1) AS album_count,
                    COALESCE(pc.track_plays, 0),
                    COALESCE(pc.album_plays, 0),
                    pc.last_played
             FROM artist ar
             LEFT JOIN artist_play_counts pc ON pc.artist_id = ar.id
             WHERE album_count > 0
             ORDER BY album_plays DESC, track_plays DESC, ar.sort_name",
        );
        if let Some(limit) = limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ArtistRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    album_count: r.get(2)?,
                    track_plays: r.get(3)?,
                    album_plays: r.get(4)?,
                    last_played: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// An album's tracks in playback order: disc, then track number.
    pub fn album_tracks(&self, album_id: i64) -> Result<Vec<TrackRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.disc_no, t.track_no, t.title, ar.name, t.duration_ms,
                    t.codec, t.path, COALESCE(pc.play_count, 0)
             FROM track t
             JOIN artist ar ON ar.id = t.artist_id
             LEFT JOIN track_play_counts pc ON pc.track_id = t.id
             WHERE t.album_id = ?1 AND t.present = 1
             ORDER BY t.disc_no, t.track_no, t.title",
        )?;
        let rows = stmt
            .query_map(params![album_id], |r| {
                Ok(TrackRow {
                    id: r.get(0)?,
                    disc_no: r.get(1)?,
                    track_no: r.get(2)?,
                    title: r.get(3)?,
                    artist: r.get(4)?,
                    duration_ms: r.get(5)?,
                    codec: r.get(6)?,
                    path: r.get(7)?,
                    play_count: r.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if rows.is_empty() {
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
        }
        Ok(rows)
    }

    /// Fetch one album by primary key.
    ///
    /// A direct lookup rather than a filtered listing: queueing the whole
    /// library calls this once per album, and scanning the table each time
    /// would make that quadratic.
    pub fn album(&self, album_id: i64) -> Result<AlbumRow> {
        self.conn
            .query_row(
                "SELECT al.id, al.title, ar.name, al.year, al.track_count, al.disc_count,
                        al.duration_ms, al.is_compilation, al.dir_path, al.source_dirs,
                        COALESCE(pc.play_count, 0), pc.last_played
                 FROM album al
                 JOIN artist ar ON ar.id = al.album_artist_id
                 LEFT JOIN album_play_counts pc ON pc.album_id = al.id
                 WHERE al.id = ?1",
                params![album_id],
                |r| {
                    Ok(AlbumRow {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        album_artist: r.get(2)?,
                        year: r.get::<_, Option<i64>>(3)?.map(|y| y as i32),
                        track_count: r.get(4)?,
                        disc_count: r.get(5)?,
                        duration_ms: r.get(6)?,
                        is_compilation: r.get::<_, i64>(7)? != 0,
                        dir_path: r.get(8)?,
                        source_dirs: r.get(9)?,
                        play_count: r.get(10)?,
                        last_played: r.get(11)?,
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => crate::Error::NotFound {
                    kind: "album",
                    id: album_id,
                },
                other => other.into(),
            })
    }

    /// The album's ReplayGain: gain in dB and peak as a linear value.
    ///
    /// Tagged values win over measured ones. Whoever wrote the tags may have
    /// used a different reference level, and silently overriding their metadata
    /// would be presumptuous; our own measurement is the fallback for the great
    /// majority of records that carry no tags at all.
    pub fn album_replaygain(&self, album_id: i64) -> Result<(Option<f64>, Option<f64>)> {
        let (tagged_gain, tagged_peak): (Option<f64>, Option<f64>) = self.conn.query_row(
            "SELECT AVG(rg_album_gain), MAX(rg_album_peak)
             FROM track WHERE album_id = ?1 AND rg_album_gain IS NOT NULL",
            params![album_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if tagged_gain.is_some() {
            return Ok((tagged_gain, tagged_peak));
        }

        let measured = self
            .conn
            .query_row(
                "SELECT rg_gain_db, rg_peak FROM album WHERE id = ?1",
                params![album_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, None));
        Ok(measured)
    }

    /// The best available cover image for an album, if there is one.
    ///
    /// A file sitting in the album folder is preferred over a fetched cover:
    /// it is what the user chose to put there.
    pub fn album_art(&self, album_id: i64) -> Result<Option<AlbumArt>> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT art_path, art_cache_path FROM album WHERE id = ?1",
                params![album_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();

        let Some((folder, cached)) = row else {
            return Ok(None);
        };
        Ok(folder
            .map(|p| AlbumArt::Folder(p.into()))
            .or_else(|| cached.map(AlbumArt::Cached)))
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        Ok(LibraryStats {
            albums: one("SELECT COUNT(*) FROM album WHERE present = 1")?,
            artists: one(
                "SELECT COUNT(DISTINCT album_artist_id) FROM album WHERE present = 1",
            )?,
            tracks: one("SELECT COUNT(*) FROM track WHERE present = 1")?,
            total_duration_ms: one(
                "SELECT COALESCE(SUM(duration_ms), 0) FROM track WHERE present = 1",
            )?,
            compilations: one(
                "SELECT COUNT(*) FROM album WHERE present = 1 AND is_compilation = 1",
            )?,
            missing_albums: one("SELECT COUNT(*) FROM album WHERE present = 0")?,
            missing_tracks: one("SELECT COUNT(*) FROM track WHERE present = 0")?,
            album_plays: one("SELECT COUNT(*) FROM album_session WHERE finished = 1")?,
            track_plays: one("SELECT COUNT(*) FROM play_event WHERE completed = 1")?,
        })
    }
}

/// A tagging problem worth a human's attention, reported by `doctor`.
#[derive(Debug, Clone)]
pub struct Anomaly {
    pub kind: &'static str,
    pub detail: String,
    pub album_id: Option<i64>,
}

impl Library {
    /// Surface albums whose identity rests on weak evidence, or whose tags will
    /// make album-order playback behave badly. This is the tool for auditing a
    /// real library against the scanner's assumptions.
    pub fn doctor(&self) -> Result<Vec<Anomaly>> {
        let mut out = Vec::new();

        let mut push = |kind: &'static str, sql: &str| -> Result<()> {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Anomaly {
                        kind,
                        album_id: r.get(0)?,
                        detail: r.get(1)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            out.extend(rows);
            Ok(())
        };

        push(
            "no-album-tag",
            "SELECT al.id, ar.name || ' — ' || al.title || '  (' || al.dir_path || ')'
             FROM album al JOIN artist ar ON ar.id = al.album_artist_id
             WHERE al.present = 1 AND al.identity_source = 'directory'
             ORDER BY al.dir_path",
        )?;

        push(
            "unknown-artist",
            "SELECT al.id, al.title || '  (' || al.dir_path || ')'
             FROM album al JOIN artist ar ON ar.id = al.album_artist_id
             WHERE al.present = 1 AND ar.name = 'Unknown Artist'
             ORDER BY al.dir_path",
        )?;

        // Missing track numbers break album order, which is the whole point.
        push(
            "missing-track-numbers",
            "SELECT al.id,
                    ar.name || ' — ' || al.title || '  (' ||
                    COUNT(*) || ' of ' || al.track_count || ' tracks unnumbered)'
             FROM track t
             JOIN album al ON al.id = t.album_id
             JOIN artist ar ON ar.id = al.album_artist_id
             WHERE t.present = 1 AND al.present = 1 AND t.track_no = 0
             GROUP BY al.id
             ORDER BY ar.sort_name, al.sort_title",
        )?;

        push(
            "duplicate-track-numbers",
            "SELECT al.id,
                    ar.name || ' — ' || al.title || '  (disc ' || t.disc_no ||
                    ' track ' || t.track_no || ' appears ' || COUNT(*) || ' times)'
             FROM track t
             JOIN album al ON al.id = t.album_id
             JOIN artist ar ON ar.id = al.album_artist_id
             WHERE t.present = 1 AND al.present = 1 AND t.track_no > 0
             GROUP BY al.id, t.disc_no, t.track_no
             HAVING COUNT(*) > 1
             ORDER BY ar.sort_name, al.sort_title",
        )?;

        // A one-track 'album' is usually a stray single or a grouping failure.
        push(
            "single-track-album",
            "SELECT al.id, ar.name || ' — ' || al.title || '  (' || al.dir_path || ')'
             FROM album al JOIN artist ar ON ar.id = al.album_artist_id
             WHERE al.present = 1 AND al.track_count = 1
             ORDER BY al.dir_path",
        )?;

        // Merging is usually right, but it is the one heuristic that can fuse
        // two genuinely different releases, so it is always worth an eyeball.
        push(
            "merged-from-several-folders",
            "SELECT al.id, ar.name || ' — ' || al.title || '  (' || al.source_dirs ||
                    ' folders under ' || al.dir_path || ')'
             FROM album al JOIN artist ar ON ar.id = al.album_artist_id
             WHERE al.present = 1 AND al.source_dirs > 1
             ORDER BY al.source_dirs DESC, ar.sort_name",
        )?;

        push(
            "no-cover-art",
            "SELECT al.id, ar.name || ' — ' || al.title || '  (' || al.dir_path || ')'
             FROM album al JOIN artist ar ON ar.id = al.album_artist_id
             WHERE al.present = 1 AND al.art_path IS NULL AND al.art_cache_path IS NULL
             ORDER BY ar.sort_name, al.sort_title",
        )?;

        // Album-first playback wants album gain; track gain alone flattens the
        // dynamics the record was mastered with.
        push(
            "no-album-replaygain",
            "SELECT al.id, ar.name || ' — ' || al.title
             FROM album al JOIN artist ar ON ar.id = al.album_artist_id
             WHERE al.present = 1
               AND al.rg_gain_db IS NULL
               AND NOT EXISTS (SELECT 1 FROM track t
                                WHERE t.album_id = al.id AND t.rg_album_gain IS NOT NULL)
             ORDER BY ar.sort_name, al.sort_title",
        )?;

        push(
            "mixed-codecs",
            "SELECT al.id,
                    ar.name || ' — ' || al.title || '  (' ||
                    GROUP_CONCAT(DISTINCT t.codec) || ')'
             FROM track t
             JOIN album al ON al.id = t.album_id
             JOIN artist ar ON ar.id = al.album_artist_id
             WHERE t.present = 1 AND al.present = 1 AND t.codec IS NOT NULL
             GROUP BY al.id
             HAVING COUNT(DISTINCT t.codec) > 1
             ORDER BY ar.sort_name, al.sort_title",
        )?;

        Ok(out)
    }
}

/// Build a LIKE pattern with wildcards escaped, so a search for `100%` does not
/// match everything.
fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_library_reports_zeroes() {
        let lib = Library::open_in_memory().unwrap();
        let s = lib.stats().unwrap();
        assert_eq!(s.albums, 0);
        assert_eq!(s.tracks, 0);
        assert_eq!(s.album_plays, 0);
        assert!(lib.doctor().unwrap().is_empty());
    }

    #[test]
    fn album_sort_parses_known_names() {
        assert_eq!(AlbumSort::parse("plays"), Some(AlbumSort::Plays));
        assert_eq!(AlbumSort::parse("LAST"), Some(AlbumSort::LastPlayed));
        assert_eq!(AlbumSort::parse("nonsense"), None);
    }

    #[test]
    fn search_wildcards_are_escaped() {
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
    }

    #[test]
    fn an_album_without_replaygain_reports_none() {
        let lib = Library::open_in_memory().unwrap();
        assert_eq!(lib.album_replaygain(1).unwrap(), (None, None));
        assert_eq!(lib.album_art(1).unwrap(), None);
    }

    /// One album with `n` tracks, for the enrichment tests below.
    fn album_fixture(lib: &Library) {
        lib.conn
            .execute("INSERT INTO artist (id, name, sort_name) VALUES (1,'A','a')", [])
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO album (id, group_key, identity_source, title, sort_title,
                                    album_artist_id, dir_path, track_count, added_at, last_seen_at)
                 VALUES (1,'k','artist+title','T','t',1,'/d',1,0,0)",
                [],
            )
            .unwrap();
    }

    #[test]
    fn a_measured_gain_is_used_when_nothing_is_tagged() {
        let lib = Library::open_in_memory().unwrap();
        album_fixture(&lib);
        lib.conn
            .execute("UPDATE album SET rg_gain_db = -7.8, rg_peak = 1.1 WHERE id = 1", [])
            .unwrap();

        let (gain, peak) = lib.album_replaygain(1).unwrap();
        assert_eq!(gain, Some(-7.8));
        assert_eq!(peak, Some(1.1));
    }

    #[test]
    fn tagged_gain_outranks_a_measured_one() {
        let lib = Library::open_in_memory().unwrap();
        album_fixture(&lib);
        lib.conn
            .execute("UPDATE album SET rg_gain_db = -7.8, rg_peak = 1.1 WHERE id = 1", [])
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO track (path, album_id, artist_id, title, rg_album_gain,
                                    rg_album_peak, mtime, size, added_at)
                 VALUES ('/d/1.mp3',1,1,'t',-4.0,0.98,0,0,0)",
                [],
            )
            .unwrap();

        let (gain, _) = lib.album_replaygain(1).unwrap();
        assert_eq!(gain, Some(-4.0), "the file's own tag wins");
    }

    #[test]
    fn folder_art_outranks_a_fetched_cover() {
        let lib = Library::open_in_memory().unwrap();
        album_fixture(&lib);
        lib.conn
            .execute("UPDATE album SET art_cache_path = '1.jpg' WHERE id = 1", [])
            .unwrap();
        assert_eq!(
            lib.album_art(1).unwrap(),
            Some(AlbumArt::Cached("1.jpg".into()))
        );

        lib.conn
            .execute("UPDATE album SET art_path = '/d/cover.jpg' WHERE id = 1", [])
            .unwrap();
        assert_eq!(
            lib.album_art(1).unwrap(),
            Some(AlbumArt::Folder("/d/cover.jpg".into()))
        );
    }

    #[test]
    fn a_cached_cover_resolves_against_whichever_cache_is_configured() {
        // The same database served from a host and from a container must find
        // its covers in each place.
        let art = AlbumArt::Cached("42.jpg".into());
        assert_eq!(
            art.resolve(std::path::Path::new("/data/art")),
            PathBuf::from("/data/art/42.jpg")
        );
        assert_eq!(
            art.resolve(std::path::Path::new("/home/me/.cache/albumplayer/art")),
            PathBuf::from("/home/me/.cache/albumplayer/art/42.jpg")
        );
    }

    #[test]
    fn missing_album_is_reported_as_not_found() {
        let lib = Library::open_in_memory().unwrap();
        assert!(matches!(
            lib.album(42),
            Err(crate::Error::NotFound { kind: "album", id: 42 })
        ));
    }
}
