//! Fetching cover art for albums that have none.
//!
//! A library ripped without artwork has nothing for a grid view to show. Covers
//! are looked up by artist and title through MusicBrainz, then downloaded from
//! the Cover Art Archive.
//!
//! Two constraints shape this module:
//!
//! * **MusicBrainz allows one request per second** from a given client, and
//!   requires a descriptive `User-Agent`. Requests are therefore serialized and
//!   paced; there is no parallelism to be had here, and hammering the service
//!   would get the client blocked.
//! * **Nothing is written into the music folders.** Covers land in the cache
//!   directory, and the path is recorded in the database. The user's files are
//!   left exactly as they were.
//!
//! Albums that come back empty are marked as checked so a later run does not
//! repeat the same fruitless lookups.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use albumplayer_core::Library;
use albumplayer_core::util::now_unix;
use rusqlite::params;

use crate::{Error, Result};

/// MusicBrainz requires a real contact string. Theirs is a small, donated
/// service; identifying the client is the price of using it.
const USER_AGENT: &str = concat!(
    "AlbumPlayer/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/albumplayer )"
);

/// The documented rate limit, with a little headroom.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(1100);

/// MusicBrainz answers 503 when it decides a client is asking too often, which
/// happens well before the documented limit if the service is busy. Backing off
/// and retrying recovers; hammering on gets the client blocked outright.
const MAX_ATTEMPTS: u32 = 4;
const BACKOFF_BASE: Duration = Duration::from_secs(2);

const MUSICBRAINZ: &str = "https://musicbrainz.org/ws/2";
const COVER_ART_ARCHIVE: &str = "https://coverartarchive.org";

/// Cover size to fetch. 500px is enough for a grid and a detail panel without
/// turning the cache into gigabytes.
const COVER_SIZE: &str = "500";

/// A downloaded cover is rejected above this size, which would indicate we were
/// served something other than a thumbnail.
const MAX_COVER_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Retry albums previously checked and not found.
    pub force: bool,
    /// Stop after this many albums.
    pub limit: Option<usize>,
    /// Where covers are written. Defaults to the user cache directory.
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub considered: usize,
    pub fetched: usize,
    pub not_found: usize,
    pub failed: usize,
    pub errors: Vec<(String, String)>,
    pub duration_ms: u128,
}

/// Environment override for the cover cache.
///
/// The server and the enrichment commands must agree on this directory, and in
/// a container neither the XDG variables nor `HOME` describe where the writable
/// volume is mounted. Setting this once keeps both in step.
pub const ART_DIR_ENV: &str = "ALBUMPLAYER_ART_DIR";

/// Default cover cache location.
pub fn default_cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(ART_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or_else(|| Error::Other("neither XDG_CACHE_HOME nor HOME is set".into()))?;
    Ok(base.join("albumplayer/art"))
}

/// Paces outgoing requests so the rate limit is never exceeded.
struct RateLimiter {
    last: Option<Instant>,
}

impl RateLimiter {
    fn new() -> Self {
        Self { last: None }
    }

    fn wait(&mut self) {
        if let Some(last) = self.last {
            let elapsed = last.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                std::thread::sleep(MIN_REQUEST_INTERVAL - elapsed);
            }
        }
        self.last = Some(Instant::now());
    }
}

/// Look up and download covers for albums that have none.
pub fn run(library: &Library, options: &Options) -> Result<Report> {
    let started = Instant::now();
    let cache_dir = match &options.cache_dir {
        Some(dir) => dir.clone(),
        None => default_cache_dir()?,
    };
    std::fs::create_dir_all(&cache_dir)?;

    let pending = collect_pending(library, options)?;
    let mut report = Report {
        considered: pending.len(),
        ..Default::default()
    };

    let agent = ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .new_agent();
    let mut limiter = RateLimiter::new();

    for album in &pending {
        let label = format!("{} — {}", album.artist, album.title);
        match fetch_one(&agent, &mut limiter, album, &cache_dir) {
            Ok(Some((file_name, release_id))) => {
                store_found(library, album.id, &file_name, &release_id)?;
                report.fetched += 1;
            }
            Ok(None) => {
                // Remember the miss so the next run does not repeat it.
                store_checked(library, album.id)?;
                report.not_found += 1;
            }
            Err(e) => {
                report.failed += 1;
                if report.errors.len() < 50 {
                    report.errors.push((label, e.to_string()));
                }
            }
        }
    }

    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

struct Pending {
    id: i64,
    artist: String,
    title: String,
    mb_album_id: Option<String>,
}

fn collect_pending(library: &Library, options: &Options) -> Result<Vec<Pending>> {
    let mut sql = String::from(
        "SELECT al.id, ar.name, al.title, al.mb_album_id
         FROM album al JOIN artist ar ON ar.id = al.album_artist_id
         WHERE al.present = 1
           AND al.art_path IS NULL
           AND al.art_cache_path IS NULL",
    );
    if !options.force {
        sql.push_str(" AND al.art_checked_at IS NULL");
    }
    sql.push_str(" ORDER BY ar.sort_name, al.sort_title");
    if let Some(limit) = options.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = library.conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Pending {
                id: r.get(0)?,
                artist: r.get(1)?,
                title: r.get(2)?,
                mb_album_id: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Record a found cover.
///
/// Only the filename is stored, never the directory: the cache sits somewhere
/// different on a host than in a container, and an absolute path here would
/// break every cover the moment the server moved.
fn store_found(library: &Library, album_id: i64, file_name: &str, release_id: &str) -> Result<()> {
    library.conn.execute(
        "UPDATE album SET art_cache_path = ?2, art_mb_release_id = ?3, art_checked_at = ?4
         WHERE id = ?1",
        params![album_id, file_name, release_id, now_unix()],
    )?;
    Ok(())
}

fn store_checked(library: &Library, album_id: i64) -> Result<()> {
    library.conn.execute(
        "UPDATE album SET art_checked_at = ?2 WHERE id = ?1",
        params![album_id, now_unix()],
    )?;
    Ok(())
}

/// Resolve one album to a cover, returning the cached path and release ID.
fn fetch_one(
    agent: &ureq::Agent,
    limiter: &mut RateLimiter,
    album: &Pending,
    cache_dir: &Path,
) -> Result<Option<(String, String)>> {
    // A release ID from the file's own tags saves a search entirely.
    let candidates: Vec<String> = match &album.mb_album_id {
        Some(id) if !id.trim().is_empty() => vec![id.clone()],
        _ => search_releases(agent, limiter, &album.artist, &album.title)?,
    };

    for release_id in candidates.iter().take(3) {
        // The archive is a separate service from MusicBrainz and is not rate
        // limited the same way, but politeness costs little.
        match download_cover(agent, release_id)? {
            Some(bytes) => {
                let file_name = format!("{}.jpg", album.id);
                std::fs::write(cache_dir.join(&file_name), &bytes)?;
                return Ok(Some((file_name, release_id.clone())));
            }
            None => continue,
        }
    }
    Ok(None)
}

/// True for statuses that mean "ask again later" rather than "no".
fn is_transient(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Delay before attempt number `attempt` (1-based), doubling each time.
fn backoff_for(attempt: u32) -> Duration {
    BACKOFF_BASE * 2u32.pow(attempt.saturating_sub(1).min(4))
}

/// Search MusicBrainz for releases matching an artist and album title.
fn search_releases(
    agent: &ureq::Agent,
    limiter: &mut RateLimiter,
    artist: &str,
    title: &str,
) -> Result<Vec<String>> {
    let query = format!(
        "artist:{} AND release:{}",
        lucene_escape(artist),
        lucene_escape(title)
    );
    let url = format!(
        "{MUSICBRAINZ}/release/?query={}&fmt=json&limit=3",
        urlencode(&query)
    );

    let mut last_status = 0;
    let mut body = None;
    for attempt in 1..=MAX_ATTEMPTS {
        limiter.wait();
        match agent.get(&url).call() {
            Ok(mut response) => {
                body = Some(
                    response
                        .body_mut()
                        .read_to_string()
                        .map_err(|e| Error::Other(format!("reading MusicBrainz response: {e}")))?,
                );
                break;
            }
            // A search that finds nothing is a normal outcome, not a failure.
            Err(ureq::Error::StatusCode(404)) => return Ok(Vec::new()),
            Err(ureq::Error::StatusCode(status)) if is_transient(status) => {
                last_status = status;
                if attempt < MAX_ATTEMPTS {
                    std::thread::sleep(backoff_for(attempt));
                }
            }
            Err(e) => return Err(Error::Other(format!("MusicBrainz: {e}"))),
        }
    }

    let Some(body) = body else {
        return Err(Error::Other(format!(
            "MusicBrainz kept answering {last_status} after {MAX_ATTEMPTS} attempts"
        )));
    };

    let parsed: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| Error::Other(format!("MusicBrainz returned invalid JSON: {e}")))?;

    Ok(parsed["releases"]
        .as_array()
        .map(|releases| {
            releases
                .iter()
                .filter_map(|r| r["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// Download the front cover for a release, if the archive has one.
fn download_cover(agent: &ureq::Agent, release_id: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("{COVER_ART_ARCHIVE}/release/{release_id}/front-{COVER_SIZE}");

    // The archive throws 500s under load just as MusicBrainz throws 503s, and
    // a transient fault should not permanently mark an album as coverless.
    let mut response = None;
    let mut last_status = 0;
    for attempt in 1..=MAX_ATTEMPTS {
        match agent.get(&url).call() {
            Ok(r) => {
                response = Some(r);
                break;
            }
            // No art for this release is the common case, not an error.
            Err(ureq::Error::StatusCode(404 | 400)) => return Ok(None),
            Err(ureq::Error::StatusCode(status)) if is_transient(status) => {
                last_status = status;
                if attempt < MAX_ATTEMPTS {
                    std::thread::sleep(backoff_for(attempt));
                }
            }
            Err(e) => return Err(Error::Other(format!("cover art archive: {e}"))),
        }
    }
    let Some(mut response) = response else {
        return Err(Error::Other(format!(
            "cover art archive kept answering {last_status} after {MAX_ATTEMPTS} attempts"
        )));
    };

    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_COVER_BYTES)
        .read_to_vec()
        .map_err(|e| Error::Other(format!("downloading cover: {e}")))?;

    // Guard against an error page being cached as though it were an image.
    if !looks_like_image(&bytes) {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Check the magic bytes of a downloaded file.
fn looks_like_image(bytes: &[u8]) -> bool {
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G'];

    bytes.starts_with(JPEG)
        || bytes.starts_with(PNG)
        || (bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

/// Escape the characters Lucene treats as syntax, so a title like
/// `Album (Remastered)` searches literally instead of erroring.
fn lucene_escape(text: &str) -> String {
    const SPECIAL: &[char] = &[
        '+', '-', '&', '|', '!', '(', ')', '{', '}', '[', ']', '^', '"', '~', '*', '?', ':', '\\',
        '/',
    ];
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        if SPECIAL.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Percent-encode a query string value.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lucene_syntax_in_titles_is_escaped() {
        // Unescaped, the parentheses and colon would be query syntax.
        assert_eq!(
            lucene_escape("Album (Remastered)"),
            r"Album \(Remastered\)"
        );
        assert_eq!(lucene_escape("AC/DC"), r"AC\/DC");
        assert_eq!(lucene_escape("Kid A"), "Kid A");
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(urlencode("Kid A"), "Kid%20A");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("plain-Text_1.0~"), "plain-Text_1.0~");
    }

    #[test]
    fn non_ascii_titles_survive_encoding() {
        // Sigur Rós, Motörhead and friends must not be mangled.
        assert_eq!(urlencode("Rós"), "R%C3%B3s");
    }

    #[test]
    fn only_real_images_are_accepted() {
        assert!(looks_like_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(looks_like_image(b"\x89PNG\r\n\x1a\n"));
        assert!(looks_like_image(b"RIFF____WEBPVP8 "));
        // An HTML error page must never be stored as a cover.
        assert!(!looks_like_image(b"<!DOCTYPE html><html>404"));
        assert!(!looks_like_image(b""));
    }

    #[test]
    fn only_retryable_statuses_are_retried() {
        // Throttling and server faults are worth another go.
        for status in [429, 500, 502, 503, 504] {
            assert!(is_transient(status), "{status}");
        }
        // A genuine "no such release" is not.
        for status in [200, 400, 401, 403, 404] {
            assert!(!is_transient(status), "{status}");
        }
    }

    #[test]
    fn backoff_grows_with_each_attempt() {
        assert_eq!(backoff_for(1), BACKOFF_BASE);
        assert_eq!(backoff_for(2), BACKOFF_BASE * 2);
        assert_eq!(backoff_for(3), BACKOFF_BASE * 4);
        assert!(backoff_for(1) < backoff_for(4));
    }

    #[test]
    fn the_rate_limiter_spaces_requests_out() {
        let mut limiter = RateLimiter::new();
        let started = Instant::now();
        limiter.wait(); // first call is free
        limiter.wait(); // second must wait out the interval
        assert!(
            started.elapsed() >= MIN_REQUEST_INTERVAL,
            "waited only {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_user_agent_identifies_the_client() {
        // MusicBrainz rejects requests from anonymous or generic clients.
        assert!(USER_AGENT.starts_with("AlbumPlayer/"));
        assert!(USER_AGENT.contains("http"));
    }
}
