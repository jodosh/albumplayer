//! System media controls: the lock screen, media keys, and the desktop's
//! now-playing widget.
//!
//! The browser gets this for free from `navigator.mediaSession`, but here the
//! audio never touches the web view, so the desktop has no idea anything is
//! playing unless the shell says so. On Linux that is MPRIS over D-Bus (which
//! is what KDE's and GNOME's lock screens read); on macOS it is Now Playing.
//!
//! Everything is driven from engine events on the Rust side rather than from
//! the UI, so the controls stay current while the window is hidden or the
//! screen is locked — exactly when they are needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use albumplayer_engine::{PlayState, PlayerControl, PlayerEvent};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tauri::Manager;

/// How far a bare "seek forward/back" moves, when the OS does not say.
const SEEK_STEP: Duration = Duration::from_secs(10);

/// What the controls display for an album, captured when the UI queues it.
#[derive(Debug, Clone)]
pub struct AlbumInfo {
    pub title: String,
    pub artist: String,
    pub cover_url: Option<String>,
    /// Track id → (title, duration in ms).
    pub tracks: HashMap<i64, (String, i64)>,
}

/// Albums the shell has been handed, by id. Shared between the commands that
/// queue albums and the thread that updates the system controls.
#[derive(Default, Clone)]
pub struct Catalogue(Arc<Mutex<HashMap<i64, AlbumInfo>>>);

impl Catalogue {
    pub fn remember(&self, id: i64, info: AlbumInfo) {
        if let Ok(mut albums) = self.0.lock() {
            albums.insert(id, info);
        }
    }

    fn get(&self, id: i64) -> Option<AlbumInfo> {
        self.0.lock().ok()?.get(&id).cloned()
    }
}

/// The OS-facing half: owns the platform controls and keeps them in step with
/// the engine.
pub struct NowPlaying {
    controls: MediaControls,
    catalogue: Catalogue,
    control: PlayerControl,
    playing: bool,
}

impl NowPlaying {
    /// Register with the OS and route its buttons to the engine.
    ///
    /// Failure is not fatal: a machine without a session bus still plays music,
    /// it just cannot be paused from the lock screen.
    pub fn new(
        app: tauri::AppHandle,
        control: PlayerControl,
        catalogue: Catalogue,
    ) -> Result<Self, souvlaki::Error> {
        let mut controls = MediaControls::new(PlatformConfig {
            display_name: "AlbumPlayer",
            dbus_name: "albumplayer",
            hwnd: None,
        })?;

        let remote = control.clone();
        controls.attach(move |event| handle(&app, &remote, event))?;
        let _ = controls.set_playback(MediaPlayback::Stopped);

        Ok(Self {
            controls,
            catalogue,
            control,
            playing: false,
        })
    }

    /// Reflect one engine event in the system controls.
    pub fn update(&mut self, event: &PlayerEvent) {
        match event {
            PlayerEvent::TrackStarted {
                album_id,
                track_id,
                title,
            } => {
                let album = self.catalogue.get(*album_id);
                let track = album.as_ref().and_then(|a| a.tracks.get(track_id));
                let duration = track
                    .map(|(_, ms)| *ms)
                    .filter(|ms| *ms > 0)
                    .map(|ms| Duration::from_millis(ms as u64));
                let _ = self.controls.set_metadata(MediaMetadata {
                    title: Some(track.map_or(title.as_str(), |(t, _)| t.as_str())),
                    album: album.as_ref().map(|a| a.title.as_str()),
                    artist: album.as_ref().map(|a| a.artist.as_str()),
                    cover_url: album.as_ref().and_then(|a| a.cover_url.as_deref()),
                    duration,
                });
                // A new track starts from zero.
                self.publish(true, Some(Duration::ZERO));
            }
            PlayerEvent::StateChanged(state) => match state {
                PlayState::Playing => self.publish(true, None),
                PlayState::Paused => self.publish(false, None),
                PlayState::Stopped => self.stop(),
            },
            PlayerEvent::QueueFinished => self.stop(),
            _ => {}
        }
    }

    fn publish(&mut self, playing: bool, position: Option<Duration>) {
        self.playing = playing;
        let position = position.unwrap_or_else(|| {
            Duration::from_millis(self.control.status().position_ms.max(0) as u64)
        });
        let progress = Some(MediaPosition(position));
        let _ = self.controls.set_playback(if playing {
            MediaPlayback::Playing { progress }
        } else {
            MediaPlayback::Paused { progress }
        });
    }

    fn stop(&mut self) {
        self.playing = false;
        let _ = self.controls.set_playback(MediaPlayback::Stopped);
    }
}

/// Act on a button pressed outside the app.
fn handle(app: &tauri::AppHandle, control: &PlayerControl, event: MediaControlEvent) {
    let position = || Duration::from_millis(control.status().position_ms.max(0) as u64);
    let result = match event {
        MediaControlEvent::Play => control.play(),
        MediaControlEvent::Pause | MediaControlEvent::Stop => control.pause(),
        MediaControlEvent::Toggle => control.toggle_pause(),
        // Track, not album: that is what a media key's "next" means.
        MediaControlEvent::Next => control.next_track(),
        MediaControlEvent::Previous => control.prev_track(),
        MediaControlEvent::SetPosition(MediaPosition(to)) => control.seek(to),
        MediaControlEvent::Seek(direction) => control.seek(step(position(), direction, SEEK_STEP)),
        MediaControlEvent::SeekBy(direction, by) => control.seek(step(position(), direction, by)),
        MediaControlEvent::Raise => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
            Ok(())
        }
        _ => Ok(()),
    };
    if let Err(e) = result {
        eprintln!("media controls: {e}");
    }
}

fn step(from: Duration, direction: SeekDirection, by: Duration) -> Duration {
    match direction {
        SeekDirection::Forward => from + by,
        SeekDirection::Backward => from.saturating_sub(by),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeking_back_stops_at_the_start() {
        let at = Duration::from_secs(4);
        assert_eq!(step(at, SeekDirection::Backward, SEEK_STEP), Duration::ZERO);
        assert_eq!(step(at, SeekDirection::Forward, SEEK_STEP), Duration::from_secs(14));
    }
}
