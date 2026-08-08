//! HTTP server for the album library.
//!
//! This is the piece that runs in the homelab. It owns the database, scans the
//! music share, measures loudness, fetches covers, serves the library, and
//! streams audio. It deliberately does **not** play anything: in a container
//! there is no sound card, and every client decodes for itself. That is what
//! lets one server serve the desktop app, a browser, and eventually a phone,
//! all sharing a single play history.

pub mod api;
pub mod auth;
pub mod config;
pub mod state;

pub use config::Config;
pub use state::AppState;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration: {0}")]
    Config(String),

    #[error("authentication required")]
    Unauthorized,

    #[error("not found")]
    NotFound,

    #[error("{0}")]
    BadRequest(String),

    #[error("internal error")]
    Internal,

    #[error(transparent)]
    Core(#[from] albumplayer_core::Error),

    #[error(transparent)]
    Enrich(#[from] albumplayer_enrich::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Error::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            Error::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            Error::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Error::Core(albumplayer_core::Error::NotFound { .. }) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            // Everything else is our problem, and the details stay in the log
            // rather than going out to a caller who may not be trusted.
            other => {
                tracing::error!(error = %other, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    }
}
