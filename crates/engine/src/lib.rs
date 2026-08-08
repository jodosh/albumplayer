//! Playback engine: the gapless, album-ordered player.
//!
//! [`queue`] is the pure model — album ordering, shuffle, and the distinction
//! between next-track and next-album. [`player`] drives GStreamer from it.

pub mod player;
pub mod playlog;
pub mod queue;

pub use player::{
    PlayState, Player, PlayerControl, PlayerEvent, PlayerStatus, album_from_library,
};
pub use playlog::PlayLogger;
pub use queue::{AlbumQueue, Cursor, QueuedAlbum, QueuedTrack, Repeat, Source};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("audio backend: {0}")]
    Backend(String),

    #[error("the playback engine has stopped")]
    EngineGone,
}

pub type Result<T> = std::result::Result<T, Error>;
