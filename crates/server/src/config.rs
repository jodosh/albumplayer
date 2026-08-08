//! Server configuration, read from the environment.
//!
//! Everything is an environment variable because the server's real home is a
//! container: `docker-compose` and Portainer both set environment cleanly,
//! whereas a config file inside an image means a bind mount for one small file.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::{Error, Result};

/// Where the music lives. Read-only as far as the server is concerned.
const MUSIC_ROOT: &str = "ALBUMPLAYER_MUSIC_ROOT";
/// Writable directory for the database and the cover cache.
const DATA_DIR: &str = "ALBUMPLAYER_DATA_DIR";
const BIND: &str = "ALBUMPLAYER_BIND";
const PASSWORD: &str = "ALBUMPLAYER_PASSWORD";
const SCAN_ON_START: &str = "ALBUMPLAYER_SCAN_ON_START";
/// How long a login stays valid, in hours.
const SESSION_HOURS: &str = "ALBUMPLAYER_SESSION_HOURS";
/// Directory holding the built web UI.
const UI_DIR: &str = "ALBUMPLAYER_UI_DIR";

#[derive(Debug, Clone)]
pub struct Config {
    pub music_root: PathBuf,
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    pub password: String,
    pub scan_on_start: bool,
    pub session_ttl: std::time::Duration,
    /// Built web UI to serve, if present. Absent means API only.
    pub ui_dir: Option<PathBuf>,
}

impl Config {
    /// Read configuration, failing loudly on anything missing or nonsensical.
    ///
    /// There is deliberately no default password. A music server that ships
    /// with a known credential and is then exposed to the internet is a
    /// liability, so refusing to start is the correct behaviour.
    pub fn from_env() -> Result<Self> {
        let password = std::env::var(PASSWORD).unwrap_or_default();
        if password.trim().is_empty() {
            return Err(Error::Config(format!(
                "{PASSWORD} must be set; refusing to start without a password"
            )));
        }
        if password.chars().count() < 8 {
            return Err(Error::Config(format!(
                "{PASSWORD} must be at least 8 characters"
            )));
        }

        let music_root = PathBuf::from(env_or(MUSIC_ROOT, "/music"));
        let data_dir = PathBuf::from(env_or(DATA_DIR, "/data"));

        let bind: SocketAddr = env_or(BIND, "0.0.0.0:8080")
            .parse()
            .map_err(|e| Error::Config(format!("{BIND} is not a socket address: {e}")))?;

        let hours: u64 = env_or(SESSION_HOURS, "720")
            .parse()
            .map_err(|e| Error::Config(format!("{SESSION_HOURS} is not a number: {e}")))?;

        // Serving the UI is optional: the API is useful on its own, and a
        // missing bundle should not stop the server booting.
        let ui_dir = PathBuf::from(env_or(UI_DIR, "/app/ui"));
        let ui_dir = ui_dir.join("index.html").is_file().then_some(ui_dir);

        Ok(Self {
            music_root,
            data_dir,
            bind,
            password,
            scan_on_start: truthy(&env_or(SCAN_ON_START, "true")),
            session_ttl: std::time::Duration::from_secs(hours * 3600),
            ui_dir,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }

    /// Where fetched covers live.
    ///
    /// Overridable so the server and the `albumplayer artwork` command can be
    /// pointed at the same directory inside a container.
    pub fn art_cache_dir(&self) -> PathBuf {
        std::env::var_os(albumplayer_enrich::artwork::ART_DIR_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.data_dir.join("art"))
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Accept the spellings people actually write in a compose file.
fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_style_booleans_are_understood() {
        for yes in ["1", "true", "TRUE", "yes", "On", " true "] {
            assert!(truthy(yes), "{yes}");
        }
        for no in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!truthy(no), "{no}");
        }
    }

    #[test]
    fn paths_are_derived_from_the_data_directory() {
        let config = Config {
            music_root: "/music".into(),
            data_dir: "/data".into(),
            bind: "0.0.0.0:8080".parse().unwrap(),
            password: "hunter2!!".into(),
            scan_on_start: true,
            session_ttl: std::time::Duration::from_secs(60),
            ui_dir: None,
        };
        assert_eq!(config.database_path(), PathBuf::from("/data/library.db"));
        assert_eq!(config.art_cache_dir(), PathBuf::from("/data/art"));
    }
}
