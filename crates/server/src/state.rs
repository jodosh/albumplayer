//! Shared server state.

use std::sync::{Arc, Mutex};

use albumplayer_core::Library;

use crate::auth::Auth;
use crate::{Config, Error, Result};

/// Everything a request handler needs.
///
/// The library is behind a mutex because a rusqlite `Connection` is `Send` but
/// not `Sync`. For a single-household server that is ample; if contention ever
/// shows up, this is the seam where a connection pool would go.
#[derive(Clone)]
pub struct AppState {
    pub library: Arc<Mutex<Library>>,
    pub auth: Arc<Auth>,
    pub config: Arc<Config>,
}

impl AppState {
    /// Run a database operation on the blocking pool.
    ///
    /// SQLite calls block, and blocking inside an async handler would stall the
    /// runtime's worker threads, so every query goes through here.
    pub async fn db<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&Library) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let library = Arc::clone(&self.library);
        tokio::task::spawn_blocking(move || {
            let guard = library.lock().map_err(|_| Error::Internal)?;
            work(&guard)
        })
        .await
        .map_err(|_| Error::Internal)?
    }
}
