//! The album server: library, covers, audio streams, and play history.
//!
//! Intended to run as a container in a homelab, with the music share mounted
//! read-only and a writable volume for the database and cover cache.

use std::sync::{Arc, Mutex};

use albumplayer_core::{Library, scan};
use albumplayer_server::{AppState, Config, api, auth::Auth};
use anyhow::{Context, Result};
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "albumplayer_server=info,tower_http=warn".into()),
        )
        .init();

    let config = Config::from_env().context("reading configuration")?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating data directory {}", config.data_dir.display()))?;

    if !config.music_root.is_dir() {
        anyhow::bail!(
            "music root {} is not a directory; is the volume mounted?",
            config.music_root.display()
        );
    }

    ensure_writable(&config.data_dir)?;

    let mut library =
        Library::open(&config.database_path()).context("opening the library database")?;
    library
        .add_root(&config.music_root)
        .context("registering the music root")?;

    if config.scan_on_start {
        tracing::info!(root = %config.music_root.display(), "scanning");
        let report = scan::scan_library(&mut library, scan::ScanOptions::default())
            .context("scanning the music root")?;
        tracing::info!(
            files = report.files_seen,
            parsed = report.files_parsed,
            unchanged = report.files_cached,
            failed = report.files_failed,
            albums = report.albums,
            new = report.albums_new,
            seconds = report.duration_ms / 1000,
            "scan complete"
        );
        if report.absences_skipped {
            tracing::warn!(
                failed = report.files_failed,
                seen = report.files_seen,
                "too many unreadable files; nothing was marked missing. Storage problem?"
            );
        }
    }

    let auth = Auth::new(&config.password, config.session_ttl)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("preparing authentication")?;

    let state = AppState {
        library: Arc::new(Mutex::new(library)),
        auth: Arc::new(auth),
        config: Arc::new(config.clone()),
    };

    let app = api::routes(state).layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("binding {}", config.bind))?;
    tracing::info!(address = %config.bind, "listening");

    // `into_make_service_with_connect_info` is what makes the peer address
    // available to handlers; the login limiter counts failures against it.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving")?;

    Ok(())
}

/// Check the data directory can actually be written to.
///
/// SQLite reports a read-only directory as "unable to open database file",
/// which says nothing about the cause. In a container the cause is almost
/// always a volume owned by root while the server runs unprivileged, so name
/// that possibility and the command that fixes it.
fn ensure_writable(dir: &std::path::Path) -> Result<()> {
    let probe = dir.join(".write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => {
            let uid = unsafe { libc::getuid() };
            anyhow::bail!(
                "cannot write to {} (running as uid {uid}): {e}\n\
                 If this is a container, the volume is probably owned by root. Fix it with:\n\
                 \x20 docker run --rm -u 0 -v <volume>:/data alpine chown -R {uid}:{uid} /data",
                dir.display()
            )
        }
    }
}

/// Stop cleanly on Ctrl-C or on the SIGTERM that Docker sends.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
