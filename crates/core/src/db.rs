//! SQLite schema and connection handling.
//!
//! Two design rules hold this schema together:
//!
//! 1. **Plays are events, never counters.** `play_event` and `album_session`
//!    are append-only; every "how many times" question is a view over them.
//!    Counters would be a one-way door — events let us answer questions we
//!    have not thought of yet.
//! 2. **Scanning never deletes.** Files that disappear flip `present` to 0
//!    instead of being removed, so reorganizing the library on disk cannot
//!    destroy listening history.

use std::path::Path;

use rusqlite::Connection;

use crate::Result;

const SCHEMA_VERSION: i64 = 5;

const SCHEMA: &str = r#"
CREATE TABLE artist (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    sort_name  TEXT    NOT NULL,
    mbid       TEXT
);

CREATE TABLE album (
    id              INTEGER PRIMARY KEY,
    group_key       TEXT    NOT NULL UNIQUE,
    identity_source TEXT    NOT NULL,
    title           TEXT    NOT NULL,
    sort_title      TEXT    NOT NULL,
    album_artist_id INTEGER NOT NULL REFERENCES artist(id),
    year            INTEGER,
    mb_album_id     TEXT,
    dir_path        TEXT    NOT NULL,
    disc_count      INTEGER NOT NULL DEFAULT 1,
    track_count     INTEGER NOT NULL DEFAULT 0,
    duration_ms     INTEGER NOT NULL DEFAULT 0,
    art_path        TEXT,
    is_compilation  INTEGER NOT NULL DEFAULT 0,
    -- Directories this album was assembled from; >1 means folders were merged.
    source_dirs     INTEGER NOT NULL DEFAULT 1,
    present         INTEGER NOT NULL DEFAULT 1,

    -- Enrichment. None of these are written by the scanner, so they survive a
    -- rescan; measuring loudness and fetching covers is far too expensive to
    -- redo every time the library is walked.
    rg_gain_db        REAL,
    rg_peak           REAL,
    rg_measured_at    INTEGER,
    -- Bare filename, not a path: the cover cache sits at a different absolute
    -- location on a host than inside a container, and the database has to
    -- survive moving between them.
    art_cache_path    TEXT,
    art_checked_at    INTEGER,
    art_mb_release_id TEXT,

    added_at        INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL
);
CREATE INDEX album_artist_idx ON album(album_artist_id);
CREATE INDEX album_sort_idx   ON album(sort_title);
CREATE INDEX album_present_idx ON album(present);

CREATE TABLE track (
    id            INTEGER PRIMARY KEY,
    path          TEXT    NOT NULL UNIQUE,
    album_id      INTEGER NOT NULL REFERENCES album(id),
    artist_id     INTEGER NOT NULL REFERENCES artist(id),
    disc_no       INTEGER NOT NULL DEFAULT 1,
    track_no      INTEGER NOT NULL DEFAULT 0,
    title         TEXT    NOT NULL,
    duration_ms   INTEGER NOT NULL DEFAULT 0,
    codec         TEXT,
    bitrate       INTEGER,
    sample_rate   INTEGER,
    rg_track_gain REAL,
    rg_track_peak REAL,
    rg_album_gain REAL,
    rg_album_peak REAL,

    -- Raw tag values retained so a rescan can regroup albums straight from
    -- the database without re-reading tags off unchanged files.
    album_raw        TEXT,
    album_artist_raw TEXT,
    artist_raw       TEXT,
    mb_album_id      TEXT,
    compilation      INTEGER NOT NULL DEFAULT 0,
    year             INTEGER,

    mtime    INTEGER NOT NULL,
    size     INTEGER NOT NULL,
    present  INTEGER NOT NULL DEFAULT 1,
    added_at INTEGER NOT NULL
);
CREATE INDEX track_album_idx  ON track(album_id, disc_no, track_no);
CREATE INDEX track_artist_idx ON track(artist_id);

-- One row per listen. `completed` follows the Last.fm rule: more than half the
-- track, or more than four minutes, whichever comes first.
CREATE TABLE play_event (
    id         INTEGER PRIMARY KEY,
    track_id   INTEGER NOT NULL REFERENCES track(id),
    album_id   INTEGER NOT NULL REFERENCES album(id),
    -- Sessions may be pruned; the individual listen still stands on its own.
    session_id INTEGER REFERENCES album_session(id) ON DELETE SET NULL,
    started_at INTEGER NOT NULL,
    ms_played  INTEGER NOT NULL DEFAULT 0,
    completed  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX play_event_track_idx   ON play_event(track_id);
CREATE INDEX play_event_album_idx   ON play_event(album_id);
CREATE INDEX play_event_started_idx ON play_event(started_at);

-- One row per sitting with an album. `finished` marks a full listen-through,
-- which is the statistic this player actually cares about.
CREATE TABLE album_session (
    id               INTEGER PRIMARY KEY,
    album_id         INTEGER NOT NULL REFERENCES album(id),
    started_at       INTEGER NOT NULL,
    ended_at         INTEGER,
    tracks_completed INTEGER NOT NULL DEFAULT 0,
    finished         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX album_session_album_idx   ON album_session(album_id);
CREATE INDEX album_session_started_idx ON album_session(started_at);

CREATE TABLE library_root (
    id       INTEGER PRIMARY KEY,
    path     TEXT NOT NULL UNIQUE,
    added_at INTEGER NOT NULL
);

CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Derived play statistics. Never materialized, always recomputable.
CREATE VIEW album_play_counts AS
    SELECT album_id,
           COUNT(*)         AS play_count,
           MAX(started_at)  AS last_played
    FROM album_session
    WHERE finished = 1
    GROUP BY album_id;

CREATE VIEW track_play_counts AS
    SELECT track_id,
           COUNT(*)        AS play_count,
           MAX(started_at) AS last_played
    FROM play_event
    WHERE completed = 1
    GROUP BY track_id;

CREATE VIEW artist_play_counts AS
    SELECT a.id AS artist_id,
           (SELECT COUNT(*) FROM play_event pe
              JOIN track t ON t.id = pe.track_id
             WHERE t.artist_id = a.id AND pe.completed = 1) AS track_plays,
           (SELECT COUNT(*) FROM album_session s
              JOIN album al ON al.id = s.album_id
             WHERE al.album_artist_id = a.id AND s.finished = 1) AS album_plays,
           (SELECT MAX(pe.started_at) FROM play_event pe
              JOIN track t ON t.id = pe.track_id
             WHERE t.artist_id = a.id) AS last_played
    FROM artist a;
"#;

/// An open library database.
pub struct Library {
    pub conn: Connection,
}

impl Library {
    /// Open (creating if needed) the library at `path`, applying migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    /// An in-memory library, used by tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Enrichment jobs and the player can hold the database open at the same
        // time; WAL allows one writer, so waiting beats failing.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;

        let mut lib = Self { conn };
        lib.migrate()?;
        Ok(lib)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 =
            self.conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if version == 0 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            return Ok(());
        }
        if version > SCHEMA_VERSION {
            return Err(crate::Error::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        // Upgrades run in order, each bumping user_version, so a database can
        // be brought forward from any older release without a rescan. Rescanning
        // an 80 GB library is minutes of work; losing measured loudness is hours.
        if version < 2 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(
                "ALTER TABLE album ADD COLUMN rg_gain_db        REAL;
                 ALTER TABLE album ADD COLUMN rg_peak           REAL;
                 ALTER TABLE album ADD COLUMN rg_measured_at    INTEGER;
                 ALTER TABLE album ADD COLUMN art_cache_path    TEXT;
                 ALTER TABLE album ADD COLUMN art_checked_at    INTEGER;
                 ALTER TABLE album ADD COLUMN art_mb_release_id TEXT;",
            )?;
            tx.pragma_update(None, "user_version", 2)?;
            tx.commit()?;
        }

        // v3 stores cached covers as bare filenames. Absolute paths written by
        // v2 break the moment the server runs somewhere with a different cache
        // directory — a container, most obviously.
        if version < 3 {
            let tx = self.conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "SELECT id, art_cache_path FROM album WHERE art_cache_path IS NOT NULL",
                )?;
                let rows: Vec<(i64, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                for (id, stored) in rows {
                    if let Some(name) = Path::new(&stored).file_name().and_then(|n| n.to_str()) {
                        tx.execute(
                            "UPDATE album SET art_cache_path = ?2 WHERE id = ?1",
                            rusqlite::params![id, name],
                        )?;
                    }
                }
            }
            tx.pragma_update(None, "user_version", 3)?;
            tx.commit()?;
        }

        // v4 makes directory-derived identities relative to the library root.
        // Absolute keys tie an album to where the library is mounted, so moving
        // the server into a container would orphan every album that has no
        // usable album tag — along with its measured loudness and its cover.
        if version < 4 {
            let tx = self.conn.transaction()?;
            {
                let roots: Vec<String> = tx
                    .prepare("SELECT path FROM library_root")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<_, _>>()?;

                let mut stmt =
                    tx.prepare("SELECT id, group_key FROM album WHERE group_key LIKE 'dir:%'")?;
                let rows: Vec<(i64, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;

                for (id, key) in rows {
                    let path = key.trim_start_matches("dir:");
                    let Some(relative) = roots
                        .iter()
                        .find_map(|root| Path::new(path).strip_prefix(root).ok())
                    else {
                        continue;
                    };
                    let updated = format!("dir:{}", relative.to_string_lossy());
                    // A collision would mean two folders reduced to one key; keep
                    // the existing row rather than failing the whole migration.
                    let _ = tx.execute(
                        "UPDATE album SET group_key = ?2 WHERE id = ?1",
                        rusqlite::params![id, updated],
                    );
                }
            }
            tx.pragma_update(None, "user_version", 4)?;
            tx.commit()?;
        }

        // v5 catches the other place a directory ends up inside an identity:
        // albums whose title was truncated by ID3v1 carry a `|dir:` suffix to
        // keep unrelated releases apart, and that suffix was absolute too.
        if version < 5 {
            let tx = self.conn.transaction()?;
            {
                let roots: Vec<String> = tx
                    .prepare("SELECT path FROM library_root")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<_, _>>()?;

                let mut stmt =
                    tx.prepare("SELECT id, group_key FROM album WHERE group_key LIKE '%dir:%'")?;
                let rows: Vec<(i64, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;

                for (id, key) in rows {
                    let Some((prefix, path)) = key.rsplit_once("dir:") else {
                        continue;
                    };
                    let Some(relative) = roots
                        .iter()
                        .find_map(|root| Path::new(path).strip_prefix(root).ok())
                    else {
                        continue; // already relative, or from another library
                    };
                    let updated = format!("{prefix}dir:{}", relative.to_string_lossy());
                    let _ = tx.execute(
                        "UPDATE album SET group_key = ?2 WHERE id = ?1",
                        rusqlite::params![id, updated],
                    );
                }
            }
            tx.pragma_update(None, "user_version", 5)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Register a directory to be scanned. Idempotent.
    pub fn add_root(&self, path: &Path) -> Result<()> {
        let canonical = path.canonicalize()?;
        self.conn.execute(
            "INSERT INTO library_root(path, added_at) VALUES (?1, ?2)
             ON CONFLICT(path) DO NOTHING",
            rusqlite::params![canonical.to_string_lossy(), crate::util::now_unix()],
        )?;
        Ok(())
    }

    pub fn roots(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM library_root ORDER BY path")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_is_at_current_version() {
        let lib = Library::open_in_memory().unwrap();
        let v: i64 = lib
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn views_exist_and_are_queryable() {
        let lib = Library::open_in_memory().unwrap();
        for view in ["album_play_counts", "track_play_counts", "artist_play_counts"] {
            let sql = format!("SELECT COUNT(*) FROM {view}");
            let n: i64 = lib.conn.query_row(&sql, [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0);
        }
    }

    #[test]
    fn an_older_database_is_upgraded_without_losing_data() {
        // Build a v1-shaped album table, then open it and confirm the upgrade
        // adds the enrichment columns while keeping the row.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artist (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                                  sort_name TEXT NOT NULL, mbid TEXT);
             CREATE TABLE album (id INTEGER PRIMARY KEY, group_key TEXT NOT NULL UNIQUE,
                                 identity_source TEXT NOT NULL, title TEXT NOT NULL,
                                 sort_title TEXT NOT NULL, album_artist_id INTEGER NOT NULL,
                                 year INTEGER, mb_album_id TEXT, dir_path TEXT NOT NULL,
                                 disc_count INTEGER NOT NULL DEFAULT 1,
                                 track_count INTEGER NOT NULL DEFAULT 0,
                                 duration_ms INTEGER NOT NULL DEFAULT 0, art_path TEXT,
                                 is_compilation INTEGER NOT NULL DEFAULT 0,
                                 source_dirs INTEGER NOT NULL DEFAULT 1,
                                 present INTEGER NOT NULL DEFAULT 1,
                                 added_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);
             CREATE TABLE library_root (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                                        added_at INTEGER NOT NULL);
             INSERT INTO artist (id, name, sort_name) VALUES (1, 'A', 'a');
             INSERT INTO album (id, group_key, identity_source, title, sort_title,
                                album_artist_id, dir_path, added_at, last_seen_at)
             VALUES (1, 'k', 'artist+title', 'T', 't', 1, '/d', 0, 0);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();

        let lib = Library::from_conn(conn).unwrap();
        let version: i64 = lib
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The pre-existing row survived, and the new columns are queryable.
        let (title, gain): (String, Option<f64>) = lib
            .conn
            .query_row("SELECT title, rg_gain_db FROM album WHERE id = 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(title, "T");
        assert_eq!(gain, None);
    }

    #[test]
    fn absolute_cover_paths_are_reduced_to_filenames() {
        let lib = Library::open_in_memory().unwrap();
        lib.conn
            .execute("INSERT INTO artist (id, name, sort_name) VALUES (1,'A','a')", [])
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO album (id, group_key, identity_source, title, sort_title,
                                    album_artist_id, dir_path, added_at, last_seen_at,
                                    art_cache_path)
                 VALUES (1,'k','artist+title','T','t',1,'/d',0,0,'/home/me/.cache/albumplayer/art/1.jpg')",
                [],
            )
            .unwrap();
        // Pretend this row was written before the v3 migration.
        lib.conn.pragma_update(None, "user_version", 2).unwrap();

        let mut lib = lib;
        lib.migrate().unwrap();

        let stored: String = lib
            .conn
            .query_row("SELECT art_cache_path FROM album WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, "1.jpg", "the cache directory must not be baked in");
    }

    #[test]
    fn directory_identities_are_made_relative_by_migration() {
        let lib = Library::open_in_memory().unwrap();
        lib.conn
            .execute("INSERT INTO artist (id, name, sort_name) VALUES (1,'A','a')", [])
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO library_root (path, added_at) VALUES ('/mnt/share/Music', 0)",
                [],
            )
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO album (id, group_key, identity_source, title, sort_title,
                                    album_artist_id, dir_path, added_at, last_seen_at)
                 VALUES (1,'dir:/mnt/share/Music/Bootleg','directory','B','b',1,
                         '/mnt/share/Music/Bootleg',0,0)",
                [],
            )
            .unwrap();
        lib.conn.pragma_update(None, "user_version", 3).unwrap();

        let mut lib = lib;
        lib.migrate().unwrap();

        let key: String = lib
            .conn
            .query_row("SELECT group_key FROM album WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(key, "dir:Bootleg", "identity must not depend on the mount point");
    }

    #[test]
    fn truncated_title_identities_are_made_relative_too() {
        let lib = Library::open_in_memory().unwrap();
        lib.conn
            .execute("INSERT INTO artist (id, name, sort_name) VALUES (1,'A','a')", [])
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO library_root (path, added_at) VALUES ('/mnt/share/Music', 0)",
                [],
            )
            .unwrap();
        lib.conn
            .execute(
                "INSERT INTO album (id, group_key, identity_source, title, sort_title,
                                    album_artist_id, dir_path, added_at, last_seen_at)
                 VALUES (1,'aa:beck|a western harvest field by m..||dir:/mnt/share/Music/Beck/AWHFBM',
                         'artist+title','T','t',1,'/mnt/share/Music/Beck/AWHFBM',0,0)",
                [],
            )
            .unwrap();
        lib.conn.pragma_update(None, "user_version", 4).unwrap();

        let mut lib = lib;
        lib.migrate().unwrap();

        let key: String = lib
            .conn
            .query_row("SELECT group_key FROM album WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(key, "aa:beck|a western harvest field by m..||dir:Beck/AWHFBM");
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_downgraded() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            Library::from_conn(conn),
            Err(crate::Error::SchemaTooNew { .. })
        ));
    }
}
