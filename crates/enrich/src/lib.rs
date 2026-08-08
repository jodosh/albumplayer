//! Filling in what the files themselves do not carry: album loudness and cover
//! art. Both are stored in the library database, never written into the user's
//! music.

pub mod artwork;
pub mod replaygain;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("library error: {0}")]
    Core(#[from] albumplayer_core::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
