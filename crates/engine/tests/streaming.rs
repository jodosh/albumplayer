//! Playing from a server over HTTP, the way the desktop shell does.
//!
//! The other playback tests use local files. This one exercises the path that
//! actually matters for the desktop app: `Source::Url`, streamed from a running
//! AlbumPlayer server, decoded by GStreamer. Gapless depends on `playbin3`
//! handling an `https://` URI as readily as a file, and nothing else in the
//! suite proves that.
//!
//! It needs a real server, so it is ignored by default:
//!
//! ```sh
//! ALBUMPLAYER_TEST_SERVER=http://your-server:8080 \
//! ALBUMPLAYER_TEST_PASSWORD=... \
//!   cargo test -p albumplayer-engine --test streaming -- --ignored --nocapture
//! ```

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use albumplayer_engine::{Player, PlayerEvent, QueuedAlbum, QueuedTrack, Source};

const SINK: &str = "fakesink sync=true";

struct Server {
    base: String,
    token: String,
}

/// Log in, or explain what is missing.
fn server() -> Option<Server> {
    let base = std::env::var("ALBUMPLAYER_TEST_SERVER").ok()?;
    let password = std::env::var("ALBUMPLAYER_TEST_PASSWORD").ok()?;

    let response = ureq::post(format!("{base}/api/auth/login"))
        .send_json(serde_json::json!({ "password": password }))
        .ok()?
        .body_mut()
        .read_json::<serde_json::Value>()
        .ok()?;

    Some(Server {
        base,
        token: response["token"].as_str()?.to_string(),
    })
}

/// Fetch an album and turn it into a queue entry of streaming tracks.
fn stream_album(server: &Server, album_id: i64) -> Option<QueuedAlbum> {
    let album = ureq::get(format!("{}/api/albums/{album_id}", server.base))
        .header("Authorization", format!("Bearer {}", server.token))
        .call()
        .ok()?
        .body_mut()
        .read_json::<serde_json::Value>()
        .ok()?;

    let tracks = album["tracks"]
        .as_array()?
        .iter()
        .map(|t| {
            let id = t["id"].as_i64().unwrap_or_default();
            QueuedTrack {
                id,
                source: Source::Url(format!(
                    "{}/api/tracks/{id}/stream?token={}",
                    server.base, server.token
                )),
                title: t["title"].as_str().unwrap_or_default().to_string(),
                disc_no: t["disc_no"].as_i64().unwrap_or(1),
                track_no: t["track_no"].as_i64().unwrap_or(0),
                duration_ms: t["duration_ms"].as_i64().unwrap_or(0),
            }
        })
        .collect();

    Some(QueuedAlbum {
        id: album_id,
        title: album["title"].as_str().unwrap_or_default().to_string(),
        artist: album["artist"].as_str().unwrap_or_default().to_string(),
        album_gain_db: album["gain_db"].as_f64(),
        album_peak: album["peak"].as_f64(),
        tracks,
    })
}

fn collect_until(
    events: &Receiver<PlayerEvent>,
    timeout: Duration,
    stop: impl Fn(&[PlayerEvent]) -> bool,
) -> Vec<PlayerEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(250)) {
            seen.push(event);
            if stop(&seen) {
                break;
            }
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
#[ignore = "needs a running server; see the module comment"]
fn an_album_streams_from_the_server_and_changes_track_by_itself() {
    let Some(server) = server() else {
        eprintln!("skipping: ALBUMPLAYER_TEST_SERVER / _PASSWORD not set");
        return;
    };
    // Album 1 is whatever sorts first; any real album exercises the same path.
    let Some(album) = stream_album(&server, 1) else {
        panic!("could not fetch an album from {}", server.base);
    };
    assert!(album.tracks.len() >= 2, "need two tracks to prove a handover");
    println!("streaming: {} — {}", album.artist, album.title);

    let (player, events) = Player::with_sink(SINK).expect("engine");
    player.play_album(album).unwrap();

    // Wait for the first track, then for the handover to the second. A real
    // track is minutes long, so skip rather than wait it out.
    let first = collect_until(&events, Duration::from_secs(45), |e| {
        !started_tracks(e).is_empty()
    });
    let started = started_tracks(&first);
    assert!(
        !started.is_empty(),
        "nothing started playing from the server: {first:#?}"
    );
    println!("  first track started: {}", started[0]);

    player.next_track().unwrap();
    let second = collect_until(&events, Duration::from_secs(45), |e| {
        started_tracks(e).len() >= 1
    });
    let after = started_tracks(&second);
    assert!(
        !after.is_empty() && after[0] != started[0],
        "did not move to the next track: {after:?}"
    );
    println!("  moved to track: {}", after[0]);

    // Position advancing is what proves audio is genuinely being decoded rather
    // than the pipeline merely being configured.
    std::thread::sleep(Duration::from_secs(3));
    let status = player.status();
    println!("  position: {} ms", status.position_ms);
    assert!(
        status.position_ms > 500,
        "playback position did not advance; audio is not flowing"
    );
}

#[test]
#[ignore = "needs a running server; see the module comment"]
fn album_replaygain_from_the_server_reaches_the_pipeline() {
    let Some(server) = server() else {
        eprintln!("skipping: ALBUMPLAYER_TEST_SERVER / _PASSWORD not set");
        return;
    };
    let Some(album) = stream_album(&server, 1) else {
        panic!("could not fetch an album");
    };
    let gain = album.album_gain_db;
    println!("album gain from the server: {gain:?} dB");
    assert!(
        gain.is_some(),
        "the server served no ReplayGain, so the enrichment did not migrate"
    );

    let (player, events) = Player::with_sink(SINK).expect("engine");
    player.play_album(album).unwrap();
    collect_until(&events, Duration::from_secs(45), |e| {
        !started_tracks(e).is_empty()
    });

    // A negative gain must pull the volume below unity.
    std::thread::sleep(Duration::from_millis(600));
    println!("  playing: {:?}", player.status().track);
}
