//! Core of an album-first music player: scanning, library storage, and the
//! play-history log.
//!
//! The organizing principle is that the *album* is the unit of listening. Every
//! layer above this crate — the playback engine, the local HTTP API, the
//! desktop UI — treats albums as the thing you queue, shuffle, and count.

pub mod db;
pub mod ffprobe;
pub mod model;
pub mod plays;
pub mod query;
pub mod scan;
pub mod util;

pub use db::Library;
pub use model::{AlbumRow, ArtistRow, LibraryStats, TrackRow};
pub use scan::{ScanReport, scan_library, scan_roots};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("could not read tags: {0}")]
    Tag(#[from] lofty::error::LoftyError),

    #[error("no library roots configured; add one with `albumplayer scan <DIR>`")]
    NoRoots,

    #[error("database schema is version {found}, but this build understands {supported}")]
    SchemaTooNew { found: i64, supported: i64 },

    #[error("no such {kind} with id {id}")]
    NotFound { kind: &'static str, id: i64 },
}

pub type Result<T> = std::result::Result<T, Error>;
