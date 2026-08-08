//! The desktop shell.
//!
//! It wraps the same web UI the server serves, but replaces the browser's audio
//! with the GStreamer engine. That is the entire reason this app exists: two
//! `<audio>` elements can only *approximate* gapless playback, and an album
//! whose tracks segue needs the real thing.
//!
//! The division of labour:
//!
//! * **Rust** owns the queue and the pipeline, and streams from the server over
//!   HTTP — `playbin3` plays an `https://` URI as happily as a local file.
//! * **The UI** owns the library, the login and the play history, talking to the
//!   server exactly as it does in a browser. Playback events come back from
//!   here so it can report what was heard.
//!
//! Keeping history on the server side is deliberate: one listening history
//! across the desktop, a browser and eventually a phone.

use std::sync::Mutex;

use albumplayer_engine::{PlayState, Player, PlayerEvent, QueuedAlbum, QueuedTrack, Repeat, Source};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

/// An album handed over from the UI, with stream URLs already signed.
#[derive(Debug, Deserialize)]
struct AlbumRequest {
    id: i64,
    title: String,
    artist: String,
    gain_db: Option<f64>,
    peak: Option<f64>,
    tracks: Vec<TrackRequest>,
}

#[derive(Debug, Deserialize)]
struct TrackRequest {
    id: i64,
    /// Full stream URL including its access token. Built by the UI so the shell
    /// never needs to know how the server authenticates.
    url: String,
    title: String,
    disc_no: i64,
    track_no: i64,
    duration_ms: i64,
}

impl From<AlbumRequest> for QueuedAlbum {
    fn from(album: AlbumRequest) -> Self {
        Self {
            id: album.id,
            title: album.title,
            artist: album.artist,
            album_gain_db: album.gain_db,
            album_peak: album.peak,
            tracks: album
                .tracks
                .into_iter()
                .map(|t| QueuedTrack {
                    id: t.id,
                    source: Source::Url(t.url),
                    title: t.title,
                    disc_no: t.disc_no,
                    track_no: t.track_no,
                    duration_ms: t.duration_ms,
                })
                .collect(),
        }
    }
}

/// What the UI renders in the transport bar.
#[derive(Debug, Serialize)]
struct Status {
    state: &'static str,
    album_id: Option<i64>,
    track_id: Option<i64>,
    track_title: Option<String>,
    album_title: Option<String>,
    artist: Option<String>,
    position_ms: i64,
    duration_ms: i64,
    shuffle: bool,
    repeat: &'static str,
    queued_albums: usize,
}

struct Engine(Mutex<Option<Player>>);

impl Engine {
    /// Run something against the player, or report that it never started.
    fn with<T>(&self, f: impl FnOnce(&Player) -> T) -> Result<T, String> {
        let guard = self.0.lock().map_err(|_| "engine lock poisoned".to_string())?;
        let player = guard
            .as_ref()
            .ok_or_else(|| "audio engine unavailable".to_string())?;
        Ok(f(player))
    }
}

macro_rules! transport {
    ($name:ident) => {
        #[tauri::command]
        fn $name(engine: State<'_, Engine>) -> Result<(), String> {
            engine.with(|p| p.$name().map_err(|e| e.to_string()))?
        }
    };
}

transport!(play);
transport!(pause);
transport!(toggle_pause);
transport!(stop);
transport!(next_track);
transport!(prev_track);
transport!(next_album);
transport!(prev_album);
transport!(clear);

#[tauri::command]
fn play_album(engine: State<'_, Engine>, album: AlbumRequest) -> Result<(), String> {
    engine.with(|p| p.play_album(album.into()).map_err(|e| e.to_string()))?
}

#[tauri::command]
fn enqueue(engine: State<'_, Engine>, album: AlbumRequest) -> Result<(), String> {
    engine.with(|p| p.enqueue(album.into()).map_err(|e| e.to_string()))?
}

#[tauri::command]
fn seek(engine: State<'_, Engine>, position_ms: u64) -> Result<(), String> {
    engine.with(|p| {
        p.seek(std::time::Duration::from_millis(position_ms))
            .map_err(|e| e.to_string())
    })?
}

#[tauri::command]
fn set_shuffle(engine: State<'_, Engine>, on: bool) -> Result<(), String> {
    engine.with(|p| p.set_shuffle(on).map_err(|e| e.to_string()))?
}

#[tauri::command]
fn set_repeat(engine: State<'_, Engine>, mode: String) -> Result<(), String> {
    let repeat = match mode.as_str() {
        "album" => Repeat::Album,
        "queue" => Repeat::Queue,
        "off" => Repeat::Off,
        other => return Err(format!("unknown repeat mode '{other}'")),
    };
    engine.with(|p| p.set_repeat(repeat).map_err(|e| e.to_string()))?
}

#[tauri::command]
fn set_volume(engine: State<'_, Engine>, volume: f64) -> Result<(), String> {
    engine.with(|p| p.set_volume(volume).map_err(|e| e.to_string()))?
}

#[tauri::command]
fn status(engine: State<'_, Engine>) -> Result<Status, String> {
    engine.with(|p| {
        let s = p.status();
        Status {
            state: match s.state {
                PlayState::Playing => "playing",
                PlayState::Paused => "paused",
                PlayState::Stopped => "stopped",
            },
            album_id: s.album.as_ref().map(|(id, _, _)| *id),
            album_title: s.album.as_ref().map(|(_, title, _)| title.clone()),
            artist: s.album.as_ref().map(|(_, _, artist)| artist.clone()),
            track_id: s.track.as_ref().map(|(id, _)| *id),
            track_title: s.track.as_ref().map(|(_, title)| title.clone()),
            position_ms: s.position_ms,
            duration_ms: s.duration_ms,
            shuffle: s.shuffle,
            repeat: match s.repeat {
                Repeat::Off => "off",
                Repeat::Album => "album",
                Repeat::Queue => "queue",
            },
            queued_albums: s.queued_albums,
        }
    })
}

/// Tells the UI to drive playback through this shell rather than `<audio>`.
#[tauri::command]
fn native_playback_available() -> bool {
    true
}

/// Environment variable that disables WebKit's DMABUF renderer.
const DMABUF_ENV: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
/// Escape hatch for anyone whose drivers are fine and wants the faster path.
const KEEP_DMABUF_ENV: &str = "ALBUMPLAYER_ENABLE_DMABUF";

/// Work around WebKitGTK failing to allocate its render buffers.
///
/// On a good many Linux systems — including the one this was developed on —
/// WebKitGTK cannot create GBM buffers and the window comes up blank, logging
/// only `Failed to create GBM buffer`. Disabling the DMABUF renderer fixes it
/// at a small cost in compositing performance, which for a mostly-static
/// interface is a trade worth making by default: an installed application
/// should not require the user to know an environment variable to see anything.
///
/// Anything already set by the user is left alone, and
/// `ALBUMPLAYER_ENABLE_DMABUF=1` opts back into the faster path.
fn apply_webkit_workaround() {
    if !cfg!(target_os = "linux") {
        return;
    }
    if std::env::var_os(DMABUF_ENV).is_some() {
        return; // the user has an opinion; respect it
    }
    if std::env::var(KEEP_DMABUF_ENV).is_ok_and(|v| v == "1" || v == "true") {
        return;
    }
    // SAFETY: called at the very top of main, before any thread exists, which
    // is the only point at which setting the environment is sound.
    unsafe { std::env::set_var(DMABUF_ENV, "1") };
}

fn main() {
    apply_webkit_workaround();

    tauri::Builder::default()
        .setup(|app| {
            // A machine with no working audio output should still show the
            // library, so a failed engine is reported rather than fatal.
            let engine = match Player::new() {
                Ok((player, events)) => {
                    forward_events(app.handle().clone(), events);
                    Some(player)
                }
                Err(e) => {
                    eprintln!("audio engine unavailable: {e}");
                    None
                }
            };
            app.manage(Engine(Mutex::new(engine)));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            play,
            pause,
            toggle_pause,
            stop,
            next_track,
            prev_track,
            next_album,
            prev_album,
            clear,
            play_album,
            enqueue,
            seek,
            set_shuffle,
            set_repeat,
            set_volume,
            status,
            native_playback_available,
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the desktop shell");
}

/// Relay engine events to the web view.
///
/// The UI needs these to write the play log: `TrackFinished` carries how much
/// was actually heard, which is what decides whether a listen counted.
fn forward_events(app: tauri::AppHandle, events: std::sync::mpsc::Receiver<PlayerEvent>) {
    std::thread::spawn(move || {
        for event in events {
            let (name, payload) = match event {
                PlayerEvent::TrackStarted {
                    album_id,
                    track_id,
                    title,
                } => (
                    "track-started",
                    serde_json::json!({ "album_id": album_id, "track_id": track_id, "title": title }),
                ),
                PlayerEvent::TrackFinished {
                    album_id,
                    track_id,
                    ms_played,
                } => (
                    "track-finished",
                    serde_json::json!({
                        "album_id": album_id, "track_id": track_id, "ms_played": ms_played
                    }),
                ),
                PlayerEvent::AlbumStarted { album_id, title } => (
                    "album-started",
                    serde_json::json!({ "album_id": album_id, "title": title }),
                ),
                PlayerEvent::StateChanged(state) => (
                    "state-changed",
                    serde_json::json!({
                        "state": match state {
                            PlayState::Playing => "playing",
                            PlayState::Paused => "paused",
                            PlayState::Stopped => "stopped",
                        }
                    }),
                ),
                PlayerEvent::QueueFinished => ("queue-finished", serde_json::json!({})),
                PlayerEvent::Error(message) => {
                    ("player-error", serde_json::json!({ "message": message }))
                }
            };
            let _ = app.emit(name, payload);
        }
    });
}
