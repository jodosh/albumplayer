//! End-to-end playback tests against a real GStreamer pipeline.
//!
//! These encode short files with ffmpeg and run them through `playbin3` with a
//! fake sink, so they exercise the actual gapless handoff without needing an
//! audio device. They are skipped when ffmpeg or the GStreamer plugins are
//! unavailable rather than failing, since neither is this crate's job to
//! provide.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use albumplayer_engine::{PlayState, Player, PlayerEvent, QueuedAlbum, QueuedTrack, Repeat, Source};

/// A fake sink that consumes audio as fast as it arrives, so a 1-second file
/// plays in a fraction of that. Fine wherever the assertion is about *ordering*.
const FAST_SINK: &str = "fakesink sync=false";

/// A fake sink that honours the clock, so a 1-second file really takes a
/// second. Needed wherever the assertion is about *timing* — elapsed playback
/// time, or landing a command mid-album.
const REALTIME_SINK: &str = "fakesink sync=true";

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Encode a short tone. Returns false if ffmpeg could not produce the file.
fn encode(path: &Path, seconds: f32) -> bool {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={seconds}"))
        .arg(path)
        .status()
        .is_ok_and(|s| s.success())
        && path.exists()
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("albumplayer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self { dir }
    }

    /// Build an album of `count` one-second tracks.
    fn album(&self, id: i64, count: usize) -> Option<QueuedAlbum> {
        let mut tracks = Vec::new();
        for i in 0..count {
            let path = self.dir.join(format!("album{id}/{:02}.opus", i + 1));
            if !encode(&path, 1.0) {
                return None;
            }
            tracks.push(QueuedTrack {
                id: id * 100 + i as i64,
                source: Source::File(path),
                title: format!("Track {}", i + 1),
                disc_no: 1,
                track_no: i as i64 + 1,
                duration_ms: 1000,
            });
        }
        Some(QueuedAlbum {
            id,
            title: format!("Album {id}"),
            artist: "Test Artist".into(),
            album_gain_db: None,
            album_peak: None,
            tracks,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Collect events until `stop` says we have enough, or we run out of patience.
fn collect_until(
    events: &Receiver<PlayerEvent>,
    timeout: Duration,
    stop: impl Fn(&[PlayerEvent]) -> bool,
) -> Vec<PlayerEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                seen.push(event);
                if stop(&seen) {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    seen
}

fn started_tracks(events: &[PlayerEvent]) -> Vec<i64> {
    events
        .iter()
        .filter_map(|e| match e {
            PlayerEvent::TrackStarted { track_id, .. } => Some(*track_id),
            _ => None,
        })
        .collect()
}

#[test]
fn an_album_plays_all_the_way_through_in_order() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let fixture = Fixture::new("inorder");
    let Some(album) = fixture.album(1, 4) else {
        eprintln!("skipping: could not encode fixtures");
        return;
    };

    let Ok((player, events)) = Player::with_sink(FAST_SINK) else {
        eprintln!("skipping: GStreamer unavailable");
        return;
    };
    player.play_album(album).unwrap();

    let seen = collect_until(&events, Duration::from_secs(30), |e| {
        e.iter().any(|e| matches!(e, PlayerEvent::QueueFinished))
    });

    // The point of the test: every track, once, in album order, with no gaps.
    assert_eq!(
        started_tracks(&seen),
        vec![100, 101, 102, 103],
        "tracks played in order; got {seen:#?}"
    );
    assert!(
        seen.iter().any(|e| matches!(e, PlayerEvent::QueueFinished)),
        "the queue reported finishing"
    );
}

#[test]
fn playback_rolls_from_one_album_into_the_next() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let fixture = Fixture::new("crossalbum");
    let (Some(first), Some(second)) = (fixture.album(1, 2), fixture.album(2, 2)) else {
        eprintln!("skipping: could not encode fixtures");
        return;
    };

    let Ok((player, events)) = Player::with_sink(FAST_SINK) else {
        eprintln!("skipping: GStreamer unavailable");
        return;
    };
    player.play_album(first).unwrap();
    player.enqueue(second).unwrap();

    let seen = collect_until(&events, Duration::from_secs(30), |e| {
        e.iter().any(|e| matches!(e, PlayerEvent::QueueFinished))
    });

    assert_eq!(started_tracks(&seen), vec![100, 101, 200, 201], "{seen:#?}");

    // Both albums announced themselves, in order.
    let albums: Vec<i64> = seen
        .iter()
        .filter_map(|e| match e {
            PlayerEvent::AlbumStarted { album_id, .. } => Some(*album_id),
            _ => None,
        })
        .collect();
    assert_eq!(albums, vec![1, 2], "each album announced once, in order");
}

#[test]
fn every_played_track_reports_how_much_was_heard() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let fixture = Fixture::new("playlog");
    let Some(album) = fixture.album(1, 3) else {
        eprintln!("skipping: could not encode fixtures");
        return;
    };

    let Ok((player, events)) = Player::with_sink(REALTIME_SINK) else {
        eprintln!("skipping: GStreamer unavailable");
        return;
    };
    player.play_album(album).unwrap();

    let seen = collect_until(&events, Duration::from_secs(30), |e| {
        e.iter().any(|e| matches!(e, PlayerEvent::QueueFinished))
    });

    let finished: Vec<(i64, i64)> = seen
        .iter()
        .filter_map(|e| match e {
            PlayerEvent::TrackFinished {
                track_id,
                ms_played,
                ..
            } => Some((*track_id, *ms_played)),
            _ => None,
        })
        .collect();

    assert_eq!(finished.len(), 3, "one report per track: {finished:?}");
    // Each report carries a duration; the play log needs it to decide whether
    // the listen counted.
    for (track_id, ms) in &finished {
        assert!(*ms > 0, "track {track_id} reported {ms}ms played");
    }
}

#[test]
fn skipping_to_the_next_album_abandons_the_rest_of_this_one() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let fixture = Fixture::new("skipalbum");
    let (Some(first), Some(second)) = (fixture.album(1, 5), fixture.album(2, 1)) else {
        eprintln!("skipping: could not encode fixtures");
        return;
    };

    let Ok((player, events)) = Player::with_sink(REALTIME_SINK) else {
        eprintln!("skipping: GStreamer unavailable");
        return;
    };
    player.play_album(first).unwrap();
    player.enqueue(second).unwrap();

    // Wait for the first album to actually start, then jump.
    collect_until(&events, Duration::from_secs(10), |e| {
        !started_tracks(e).is_empty()
    });
    player.next_album().unwrap();

    let seen = collect_until(&events, Duration::from_secs(30), |e| {
        e.iter().any(|e| matches!(e, PlayerEvent::QueueFinished))
    });

    let started = started_tracks(&seen);
    assert!(
        started.contains(&200),
        "reached the second album: {started:?}"
    );
    assert!(
        !started.contains(&104),
        "did not keep playing the abandoned album: {started:?}"
    );
}

#[test]
fn a_missing_file_does_not_end_the_session() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let fixture = Fixture::new("badfile");
    let Some(mut album) = fixture.album(1, 3) else {
        eprintln!("skipping: could not encode fixtures");
        return;
    };
    // Delete the middle track out from under the player.
    if let Source::File(path) = &album.tracks[1].source {
        let _ = std::fs::remove_file(path);
    }
    album.tracks[1].source = Source::File(fixture.dir.join("album1/missing.opus"));

    let Ok((player, events)) = Player::with_sink(FAST_SINK) else {
        eprintln!("skipping: GStreamer unavailable");
        return;
    };
    player.play_album(album).unwrap();

    let seen = collect_until(&events, Duration::from_secs(30), |e| {
        e.iter().any(|e| matches!(e, PlayerEvent::QueueFinished))
    });

    let started = started_tracks(&seen);
    assert!(started.contains(&100), "played before the gap: {started:?}");
    assert!(
        started.contains(&102),
        "recovered and played past the gap: {started:?}"
    );
}

#[test]
fn pausing_reports_the_paused_state() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let fixture = Fixture::new("pause");
    let Some(album) = fixture.album(1, 2) else {
        eprintln!("skipping: could not encode fixtures");
        return;
    };

    let Ok((player, events)) = Player::with_sink(FAST_SINK) else {
        eprintln!("skipping: GStreamer unavailable");
        return;
    };
    player.play_album(album).unwrap();
    collect_until(&events, Duration::from_secs(10), |e| {
        !started_tracks(e).is_empty()
    });

    player.pause().unwrap();
    let seen = collect_until(&events, Duration::from_secs(5), |e| {
        e.iter()
            .any(|e| matches!(e, PlayerEvent::StateChanged(PlayState::Paused)))
    });
    assert!(
        seen.iter()
            .any(|e| matches!(e, PlayerEvent::StateChanged(PlayState::Paused))),
        "paused state was reported: {seen:#?}"
    );

    // And the published snapshot agrees.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(player.status().state, PlayState::Paused);
}

#[test]
fn repeat_album_keeps_going_round() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not available");
        return;
    }
    let fixture = Fixture::new("repeat");
    let Some(album) = fixture.album(1, 2) else {
        eprintln!("skipping: could not encode fixtures");
        return;
    };

    let Ok((player, events)) = Player::with_sink(REALTIME_SINK) else {
        eprintln!("skipping: GStreamer unavailable");
        return;
    };
    player.set_repeat(Repeat::Album).unwrap();
    player.play_album(album).unwrap();

    // Five starts means it wrapped past the two-track album at least twice.
    let seen = collect_until(&events, Duration::from_secs(30), |e| {
        started_tracks(e).len() >= 5
    });
    let started = started_tracks(&seen);
    assert!(started.len() >= 5, "kept looping: {started:?}");
    assert!(
        !seen.iter().any(|e| matches!(e, PlayerEvent::QueueFinished)),
        "never reported the queue as finished"
    );
}
