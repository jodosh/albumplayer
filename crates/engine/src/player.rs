//! GStreamer playback, wrapped around the album queue.
//!
//! # How gapless works
//!
//! `playbin3` emits `about-to-finish` shortly *before* the current file runs
//! out, while there is still audio buffered. Setting its `uri` property from
//! inside that callback lets the next file be prepared and spliced on with no
//! silence between — which is the whole point for an album whose tracks segue.
//!
//! The switch itself is announced later, by a `stream-start` message on the
//! bus. That is the moment the queue cursor advances and the finished track is
//! written to the play log, so the two stay in step even though the URI was
//! handed over early.
//!
//! # Threading and locking
//!
//! A dedicated thread owns the pipeline and services commands and bus messages.
//! Only the *queue* is shared, because `about-to-finish` fires on a GStreamer
//! streaming thread and needs to know the next track.
//!
//! The one hard rule here: **never hold the queue lock across a pipeline state
//! change.** `set_state` can block until streaming threads settle, and if one of
//! those threads is sitting in `about-to-finish` waiting for the same lock, the
//! two wait on each other forever. Every handler below reads what it needs out
//! of the queue, releases the lock, and only then talks to GStreamer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::queue::{AlbumQueue, QueuedAlbum, QueuedTrack, Repeat, Source};
use crate::{Error, Result};

/// Overriding the audio sink lets tests run the real pipeline without an audio
/// device, and lets a headless server pick its own output.
const SINK_ENV: &str = "ALBUMPLAYER_AUDIO_SINK";

/// Sink used when nothing else is specified.
const DEFAULT_SINK: &str = "autoaudiosink";

/// Bus messages handled per loop iteration. Capping this matters: during fast
/// or looping playback messages can arrive faster than they are drained, and an
/// unbounded drain would starve the command channel so that even `Shutdown`
/// never gets through.
const MAX_BUS_MESSAGES_PER_TICK: usize = 32;

/// ReplayGain is applied by scaling `playbin`'s linear volume. Clamped so a
/// wildly wrong tag cannot deafen anyone.
const MAX_GAIN_SCALE: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// Things the player tells the outside world.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// A new track actually started coming out of the speakers.
    TrackStarted {
        album_id: i64,
        track_id: i64,
        title: String,
    },
    /// A track stopped, with how much of it was actually heard. This is what
    /// the play log records.
    TrackFinished {
        album_id: i64,
        track_id: i64,
        ms_played: i64,
    },
    /// Playback moved on to a different album.
    AlbumStarted { album_id: i64, title: String },
    StateChanged(PlayState),
    /// The queue ran out.
    QueueFinished,
    Error(String),
}

/// Commands accepted by the player thread.
enum Command {
    PlayAlbum(Box<QueuedAlbum>),
    Enqueue(Box<QueuedAlbum>),
    Play,
    Pause,
    TogglePause,
    Stop,
    NextTrack,
    PrevTrack,
    NextAlbum,
    PrevAlbum,
    SeekTo(Duration),
    SetShuffle(bool),
    SetRepeat(Repeat),
    SetVolume(f64),
    Clear,
    Shutdown,
}

/// A snapshot of what the player is doing, safe to read from any thread.
#[derive(Debug, Clone, Default)]
pub struct PlayerStatus {
    pub state: PlayState,
    pub album: Option<(i64, String, String)>,
    pub track: Option<(i64, String)>,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub queued_albums: usize,
}

/// Transport controls, detached from ownership of the engine thread.
///
/// This is `Send` and cloneable, so a UI can drive playback from a different
/// thread than the one consuming events — which matters because the play log
/// holds a SQLite connection that cannot be shared across threads.
#[derive(Clone)]
pub struct PlayerControl {
    commands: Sender<Command>,
    status: Arc<Mutex<PlayerStatus>>,
}

impl PlayerControl {
    fn send(&self, command: Command) -> Result<()> {
        self.commands.send(command).map_err(|_| Error::EngineGone)
    }

    /// Replace the queue with this album and start playing it.
    pub fn play_album(&self, album: QueuedAlbum) -> Result<()> {
        self.send(Command::PlayAlbum(Box::new(album)))
    }

    /// Append an album to the queue.
    pub fn enqueue(&self, album: QueuedAlbum) -> Result<()> {
        self.send(Command::Enqueue(Box::new(album)))
    }

    pub fn play(&self) -> Result<()> {
        self.send(Command::Play)
    }
    pub fn pause(&self) -> Result<()> {
        self.send(Command::Pause)
    }
    pub fn toggle_pause(&self) -> Result<()> {
        self.send(Command::TogglePause)
    }
    pub fn stop(&self) -> Result<()> {
        self.send(Command::Stop)
    }
    pub fn next_track(&self) -> Result<()> {
        self.send(Command::NextTrack)
    }
    pub fn prev_track(&self) -> Result<()> {
        self.send(Command::PrevTrack)
    }
    pub fn next_album(&self) -> Result<()> {
        self.send(Command::NextAlbum)
    }
    pub fn prev_album(&self) -> Result<()> {
        self.send(Command::PrevAlbum)
    }
    pub fn seek(&self, position: Duration) -> Result<()> {
        self.send(Command::SeekTo(position))
    }
    pub fn set_shuffle(&self, on: bool) -> Result<()> {
        self.send(Command::SetShuffle(on))
    }
    pub fn set_repeat(&self, repeat: Repeat) -> Result<()> {
        self.send(Command::SetRepeat(repeat))
    }
    pub fn set_volume(&self, volume: f64) -> Result<()> {
        self.send(Command::SetVolume(volume))
    }
    pub fn clear(&self) -> Result<()> {
        self.send(Command::Clear)
    }

    pub fn status(&self) -> PlayerStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Owns the engine thread. Shutting this down stops playback.
///
/// Derefs to [`PlayerControl`], so all the transport methods are available
/// directly; use [`Player::control`] to hand a copy to another thread.
pub struct Player {
    control: PlayerControl,
    thread: Option<JoinHandle<()>>,
}

impl std::ops::Deref for Player {
    type Target = PlayerControl;

    fn deref(&self) -> &Self::Target {
        &self.control
    }
}

impl Player {
    /// A cloneable, `Send` handle to the transport controls.
    pub fn control(&self) -> PlayerControl {
        self.control.clone()
    }
    /// Start the engine with the default audio sink.
    pub fn new() -> Result<(Self, Receiver<PlayerEvent>)> {
        let sink = std::env::var(SINK_ENV).unwrap_or_else(|_| DEFAULT_SINK.into());
        Self::with_sink(&sink)
    }

    /// Start the engine writing to a specific sink, given as a
    /// `gst-launch`-style description such as `"pulsesink"` or
    /// `"fakesink sync=true"`.
    ///
    /// Passing the sink explicitly keeps tests free of process-global state and
    /// lets a headless server choose its own output.
    pub fn with_sink(sink: &str) -> Result<(Self, Receiver<PlayerEvent>)> {
        gst::init().map_err(|e| Error::Backend(e.to_string()))?;
        let sink = sink.to_string();

        let (cmd_tx, cmd_rx) = channel();
        let (event_tx, event_rx) = channel();
        let status = Arc::new(Mutex::new(PlayerStatus::default()));

        let thread_status = Arc::clone(&status);
        let thread = std::thread::Builder::new()
            .name("albumplayer-engine".into())
            .spawn(move || {
                if let Err(e) = run(&sink, &cmd_rx, &event_tx, &thread_status) {
                    let _ = event_tx.send(PlayerEvent::Error(e.to_string()));
                }
            })
            .map_err(|e| Error::Backend(e.to_string()))?;

        Ok((
            Self {
                control: PlayerControl {
                    commands: cmd_tx,
                    status,
                },
                thread: Some(thread),
            },
            event_rx,
        ))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.control.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// State owned solely by the player thread.
struct Inner {
    playbin: gst::Element,
    state: PlayState,
    /// Volume asked for by the user, before ReplayGain is folded in.
    user_volume: f64,
    /// The track the pipeline is currently rendering, and how much of it has
    /// been audible. Tracked separately from the queue cursor because
    /// `about-to-finish` moves the queue ahead of the speakers.
    playing: Option<PlayingTrack>,
}

struct PlayingTrack {
    album_id: i64,
    track_id: i64,
    ms_played: i64,
}

/// The queue, shared with the streaming-thread callback.
type SharedQueue = Arc<Mutex<AlbumQueue>>;

/// Read the album and track the cursor points at, holding the lock only long
/// enough to clone them.
fn current(queue: &SharedQueue) -> Option<(QueuedAlbum, QueuedTrack)> {
    let q = queue.lock().ok()?;
    Some((q.current_album()?.clone(), q.current_track()?.clone()))
}

fn run(
    sink: &str,
    commands: &Receiver<Command>,
    events: &Sender<PlayerEvent>,
    status: &Arc<Mutex<PlayerStatus>>,
) -> Result<()> {
    let playbin = build_playbin(sink)?;
    let bus = playbin
        .bus()
        .ok_or_else(|| Error::Backend("playbin has no bus".into()))?;

    let queue: SharedQueue = Arc::new(Mutex::new(AlbumQueue::new()));
    // Set when a URI is handed over early, so the following `stream-start` is
    // known to be that splice rather than a seek or a deliberate jump.
    let gapless = Arc::new(AtomicBool::new(false));

    install_gapless_handoff(&playbin, &queue, &gapless);

    let mut inner = Inner {
        playbin,
        state: PlayState::Stopped,
        user_volume: 1.0,
        playing: None,
    };

    loop {
        while let Ok(command) = commands.try_recv() {
            if matches!(command, Command::Shutdown) {
                let _ = inner.playbin.set_state(gst::State::Null);
                return Ok(());
            }
            handle_command(command, &mut inner, &queue, &gapless, events);
        }

        for _ in 0..MAX_BUS_MESSAGES_PER_TICK {
            let Some(message) = bus.timed_pop(gst::ClockTime::ZERO) else {
                break;
            };
            handle_message(&message, &mut inner, &queue, &gapless, events);
        }

        tick(&mut inner, &queue, status);
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn build_playbin(sink_desc: &str) -> Result<gst::Element> {
    let playbin = gst::ElementFactory::make("playbin3")
        .build()
        .map_err(|e| Error::Backend(format!("playbin3 unavailable: {e}")))?;

    let sink = gst::parse::bin_from_description(sink_desc, true)
        .map_err(|e| Error::Backend(format!("audio sink '{sink_desc}': {e}")))?;
    playbin.set_property("audio-sink", &sink);

    Ok(playbin)
}

/// Hand the next file to the pipeline before the current one ends.
///
/// Runs on a GStreamer streaming thread. It takes the queue lock briefly to
/// read the next path, and releases it before touching the pipeline.
fn install_gapless_handoff(playbin: &gst::Element, queue: &SharedQueue, gapless: &Arc<AtomicBool>) {
    let queue = Arc::clone(queue);
    let gapless = Arc::clone(gapless);

    playbin.connect("about-to-finish", false, move |args| {
        let playbin = args[0].get::<gst::Element>().ok()?;

        let next_uri = {
            let q = queue.lock().ok()?;
            // Only the natural successor is queued here. A user skip sets the
            // URI from the command handler instead and never reaches this.
            q.peek_next()?.source.to_uri()
        };

        playbin.set_property("uri", next_uri);
        gapless.store(true, Ordering::SeqCst);
        None
    });
}

/// Apply a mutation to the queue and report whether it moved.
fn with_queue(queue: &SharedQueue, f: impl FnOnce(&mut AlbumQueue) -> bool) -> bool {
    queue.lock().map(|mut q| f(&mut q)).unwrap_or(false)
}

fn handle_command(
    command: Command,
    inner: &mut Inner,
    queue: &SharedQueue,
    gapless: &AtomicBool,
    events: &Sender<PlayerEvent>,
) {
    match command {
        Command::PlayAlbum(album) => {
            with_queue(queue, |q| {
                q.play_album(*album);
                true
            });
            start_current(inner, queue, gapless, events, true);
        }
        Command::Enqueue(album) => {
            let started = with_queue(queue, |q| {
                let was_empty = q.is_empty();
                q.enqueue(*album);
                was_empty
            });
            if started {
                start_current(inner, queue, gapless, events, true);
            }
        }
        Command::Play => set_state(inner, queue, PlayState::Playing, events),
        Command::Pause => set_state(inner, queue, PlayState::Paused, events),
        Command::TogglePause => {
            let next = match inner.state {
                PlayState::Playing => PlayState::Paused,
                _ => PlayState::Playing,
            };
            set_state(inner, queue, next, events);
        }
        Command::Stop => {
            finish_current(inner, events);
            let _ = inner.playbin.set_state(gst::State::Null);
            inner.state = PlayState::Stopped;
            let _ = events.send(PlayerEvent::StateChanged(PlayState::Stopped));
        }
        Command::NextTrack => {
            if with_queue(queue, AlbumQueue::next_track) {
                start_current(inner, queue, gapless, events, false);
            } else {
                finish_queue(inner, events);
            }
        }
        Command::PrevTrack => {
            if with_queue(queue, AlbumQueue::prev_track) {
                start_current(inner, queue, gapless, events, false);
            }
        }
        Command::NextAlbum => {
            if with_queue(queue, AlbumQueue::next_album) {
                start_current(inner, queue, gapless, events, true);
            } else {
                finish_queue(inner, events);
            }
        }
        Command::PrevAlbum => {
            if with_queue(queue, AlbumQueue::prev_album) {
                start_current(inner, queue, gapless, events, true);
            }
        }
        Command::SeekTo(position) => {
            let _ = inner.playbin.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(position.as_millis() as u64),
            );
        }
        Command::SetShuffle(on) => {
            with_queue(queue, |q| {
                q.set_shuffle(on);
                true
            });
        }
        Command::SetRepeat(repeat) => {
            with_queue(queue, |q| {
                q.set_repeat(repeat);
                true
            });
        }
        Command::SetVolume(volume) => {
            inner.user_volume = volume.clamp(0.0, 1.0);
            apply_volume(inner, queue);
        }
        Command::Clear => {
            finish_current(inner, events);
            with_queue(queue, |q| {
                q.clear();
                true
            });
            let _ = inner.playbin.set_state(gst::State::Null);
            inner.state = PlayState::Stopped;
            let _ = events.send(PlayerEvent::StateChanged(PlayState::Stopped));
        }
        Command::Shutdown => unreachable!("handled by the caller"),
    }
}

/// Begin the track the cursor points at, discarding anything in flight.
fn start_current(
    inner: &mut Inner,
    queue: &SharedQueue,
    gapless: &AtomicBool,
    events: &Sender<PlayerEvent>,
    announce_album: bool,
) {
    finish_current(inner, events);

    let Some((album, track)) = current(queue) else {
        finish_queue(inner, events);
        return;
    };

    // A deliberate jump is not a gapless splice; the resulting stream-start
    // must not advance the cursor a second time.
    gapless.store(false, Ordering::SeqCst);

    let _ = inner.playbin.set_state(gst::State::Ready);
    inner.playbin.set_property("uri", track.source.to_uri());
    apply_volume(inner, queue);
    let _ = inner.playbin.set_state(gst::State::Playing);

    inner.state = PlayState::Playing;
    inner.playing = Some(PlayingTrack {
        album_id: album.id,
        track_id: track.id,
        ms_played: 0,
    });

    if announce_album {
        let _ = events.send(PlayerEvent::AlbumStarted {
            album_id: album.id,
            title: album.title,
        });
    }
    let _ = events.send(PlayerEvent::TrackStarted {
        album_id: album.id,
        track_id: track.id,
        title: track.title,
    });
    let _ = events.send(PlayerEvent::StateChanged(PlayState::Playing));
}

/// Close out whatever was playing, reporting how much of it was heard.
fn finish_current(inner: &mut Inner, events: &Sender<PlayerEvent>) {
    if let Some(playing) = inner.playing.take() {
        let _ = events.send(PlayerEvent::TrackFinished {
            album_id: playing.album_id,
            track_id: playing.track_id,
            ms_played: playing.ms_played,
        });
    }
}

fn finish_queue(inner: &mut Inner, events: &Sender<PlayerEvent>) {
    finish_current(inner, events);
    let _ = inner.playbin.set_state(gst::State::Null);
    if inner.state != PlayState::Stopped {
        inner.state = PlayState::Stopped;
        let _ = events.send(PlayerEvent::QueueFinished);
        let _ = events.send(PlayerEvent::StateChanged(PlayState::Stopped));
    }
}

fn set_state(
    inner: &mut Inner,
    queue: &SharedQueue,
    state: PlayState,
    events: &Sender<PlayerEvent>,
) {
    if current(queue).is_none() {
        return;
    }
    let gst_state = match state {
        PlayState::Playing => gst::State::Playing,
        PlayState::Paused => gst::State::Paused,
        PlayState::Stopped => gst::State::Null,
    };
    let _ = inner.playbin.set_state(gst_state);
    inner.state = state;
    let _ = events.send(PlayerEvent::StateChanged(state));
}

/// Fold album ReplayGain into the pipeline volume.
///
/// Album gain, deliberately: track gain would even out the loud and quiet
/// moments the record was sequenced around. Peak information caps the result so
/// applying positive gain cannot clip.
fn apply_volume(inner: &Inner, queue: &SharedQueue) {
    let (gain_db, peak) = {
        let Ok(q) = queue.lock() else { return };
        match q.current_album() {
            Some(album) => (album.album_gain_db, album.album_peak),
            None => (None, None),
        }
    };
    inner
        .playbin
        .set_property("volume", volume_for(gain_db, peak, inner.user_volume));
}

/// The pipeline volume for a given album gain, peak, and user setting.
///
/// Split out from the pipeline so the maths can be checked directly. An album
/// with no gain plays at the user's volume untouched.
fn volume_for(gain_db: Option<f64>, peak: Option<f64>, user_volume: f64) -> f64 {
    let scale = match gain_db {
        Some(db) => {
            let linear = 10f64.powf(db / 20.0);
            // Never amplify a record past the point where its loudest sample
            // would clip, however much positive gain the measurement asks for.
            let peak_limit = peak.filter(|p| *p > 0.0).map_or(MAX_GAIN_SCALE, |p| 1.0 / p);
            linear.min(peak_limit).clamp(0.0, MAX_GAIN_SCALE)
        }
        None => 1.0,
    };
    (user_volume * scale).clamp(0.0, MAX_GAIN_SCALE)
}

fn handle_message(
    message: &gst::Message,
    inner: &mut Inner,
    queue: &SharedQueue,
    gapless: &AtomicBool,
    events: &Sender<PlayerEvent>,
) {
    use gst::MessageView;

    match message.view() {
        // The gapless splice actually reached the speakers: move the cursor
        // now, not when the URI was handed over.
        MessageView::StreamStart(_) => {
            if !gapless.swap(false, Ordering::SeqCst) {
                return;
            }

            let previous_album = inner.playing.as_ref().map(|p| p.album_id);
            finish_current(inner, events);

            if !with_queue(queue, AlbumQueue::advance) {
                finish_queue(inner, events);
                return;
            }
            let Some((album, track)) = current(queue) else {
                return;
            };

            if previous_album != Some(album.id) {
                apply_volume(inner, queue);
                let _ = events.send(PlayerEvent::AlbumStarted {
                    album_id: album.id,
                    title: album.title,
                });
            }

            inner.playing = Some(PlayingTrack {
                album_id: album.id,
                track_id: track.id,
                ms_played: 0,
            });
            let _ = events.send(PlayerEvent::TrackStarted {
                album_id: album.id,
                track_id: track.id,
                title: track.title,
            });
        }
        // Reached only when nothing was queued behind the current track.
        MessageView::Eos(_) => finish_queue(inner, events),
        MessageView::Error(err) => {
            let text = format!("{} ({})", err.error(), err.debug().unwrap_or_default());
            let _ = events.send(PlayerEvent::Error(text));

            // One unreadable file should not end the listening session.
            if with_queue(queue, AlbumQueue::next_track) {
                start_current(inner, queue, gapless, events, false);
            } else {
                finish_queue(inner, events);
            }
        }
        _ => {}
    }
}

/// Refresh the position clock and the published status snapshot.
fn tick(inner: &mut Inner, queue: &SharedQueue, status: &Arc<Mutex<PlayerStatus>>) {
    let position_ms = inner
        .playbin
        .query_position::<gst::ClockTime>()
        .map(|t| t.mseconds() as i64)
        .unwrap_or(0);
    let duration_ms = inner
        .playbin
        .query_duration::<gst::ClockTime>()
        .map(|t| t.mseconds() as i64)
        .unwrap_or(0);

    // Audible time, so pausing does not inflate the play log.
    if inner.state == PlayState::Playing
        && let Some(playing) = inner.playing.as_mut()
    {
        playing.ms_played = playing.ms_played.max(position_ms);
    }

    let Ok(q) = queue.lock() else { return };
    let snapshot = PlayerStatus {
        state: inner.state,
        album: q
            .current_album()
            .map(|a| (a.id, a.title.clone(), a.artist.clone())),
        track: q.current_track().map(|t| (t.id, t.title.clone())),
        position_ms,
        duration_ms,
        shuffle: q.shuffle_enabled(),
        repeat: q.repeat(),
        queued_albums: q.album_count(),
    };
    drop(q);

    if let Ok(mut published) = status.lock() {
        *published = snapshot;
    }
}

/// Build a queue album out of library rows.
pub fn album_from_library(
    album: &albumplayer_core::AlbumRow,
    tracks: &[albumplayer_core::TrackRow],
    gain: Option<f64>,
    peak: Option<f64>,
) -> QueuedAlbum {
    QueuedAlbum {
        id: album.id,
        title: album.title.clone(),
        artist: album.album_artist.clone(),
        album_gain_db: gain,
        album_peak: peak,
        tracks: tracks
            .iter()
            .map(|t| QueuedTrack {
                id: t.id,
                source: Source::File(std::path::PathBuf::from(&t.path)),
                title: t.title.clone(),
                disc_no: t.disc_no,
                track_no: t.track_no,
                duration_ms: t.duration_ms,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaygain_converts_decibels_to_a_linear_scale() {
        // -6 dB roughly halves the amplitude.
        assert!((volume_for(Some(-6.0), None, 1.0) - 0.501).abs() < 0.01);
        // A typical loud master is pulled well down.
        assert!((volume_for(Some(-7.8), None, 1.0) - 0.407).abs() < 0.01);
    }

    #[test]
    fn an_album_without_gain_plays_at_the_users_volume() {
        assert_eq!(volume_for(None, None, 1.0), 1.0);
        assert_eq!(volume_for(None, Some(1.2), 0.5), 0.5);
    }

    #[test]
    fn positive_gain_is_capped_by_the_albums_peak() {
        // +12 dB would be ~4x, but a peak of 0.5 leaves only 2x of headroom.
        let scale = volume_for(Some(12.0), Some(0.5), 1.0);
        assert!((scale - 2.0).abs() < 0.001, "{scale}");
    }

    #[test]
    fn the_user_volume_scales_the_result() {
        let full = volume_for(Some(-6.0), None, 1.0);
        let half = volume_for(Some(-6.0), None, 0.5);
        assert!((half - full / 2.0).abs() < 0.001);
    }

    #[test]
    fn volume_never_runs_away() {
        // A nonsense measurement must not blow the roof off.
        assert!(volume_for(Some(90.0), None, 1.0) <= MAX_GAIN_SCALE);
        assert!(volume_for(Some(-500.0), None, 1.0) >= 0.0);
    }

    #[test]
    fn file_paths_become_uris() {
        let uri = Source::File("/m/A B/01 - x.opus".into()).to_uri();
        assert!(uri.starts_with("file://"), "{uri}");
        assert!(uri.contains("01%20-%20x.opus"), "spaces are escaped: {uri}");
    }

    #[test]
    fn stream_urls_pass_through_untouched() {
        // A signed stream URL must reach GStreamer exactly as issued: escaping
        // it as though it were a path would corrupt the query string.
        let url = "https://home.lan/api/tracks/7/stream?token=abc%20def";
        assert_eq!(Source::Url(url.into()).to_uri(), url);
    }
}
