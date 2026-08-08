//! Last-resort tag reading via `ffprobe`.
//!
//! A handful of files in any real library are playable but too malformed for a
//! strict parser — encrypted ID3v2 frames, truncated headers. Dropping them
//! silently leaves an album with holes in its tracklist, which for an
//! album-first player is worse than the cost of spawning a process.
//!
//! This runs only for files the in-process parser has already refused, so the
//! per-file cost never applies to the bulk of a scan. If `ffprobe` is not
//! installed the file simply stays unreadable, exactly as before.

use std::path::Path;
use std::process::Command;

use crate::model::TrackMeta;
use crate::util::{leading_u32, year_from_date};

/// Read what `ffprobe` can see. Returns `None` if it is unavailable or fails.
pub fn read_tags(path: &Path, mtime: i64, size: i64) -> Option<TrackMeta> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse(&text, path, mtime, size)
}

/// Build track metadata from ffprobe's JSON.
///
/// Hand-rolled rather than pulling in a JSON dependency for one fallback path:
/// only a dozen scalar fields are needed, and they are all flat.
fn parse(json: &str, path: &Path, mtime: i64, size: i64) -> Option<TrackMeta> {
    let tag = |name: &str| tag_value(json, name);

    let duration_ms = field(json, "duration")
        .and_then(|v| v.parse::<f64>().ok())
        .map(|seconds| (seconds * 1000.0) as i64)
        .unwrap_or(0);

    // A file with no readable duration is not usable as a track.
    if duration_ms <= 0 {
        return None;
    }

    let disc_no = tag("disc").and_then(|v| leading_u32(&v)).unwrap_or(1).max(1);
    let track_no = tag("track").and_then(|v| leading_u32(&v)).unwrap_or(0);
    let year = tag("date")
        .or_else(|| tag("year"))
        .and_then(|v| year_from_date(&v));

    Some(TrackMeta {
        path: path.to_path_buf(),
        mtime,
        size,
        title: tag("title"),
        artist_raw: tag("artist"),
        album_artist_raw: tag("album_artist"),
        album_raw: tag("album"),
        mb_album_id: tag("MusicBrainz Album Id"),
        compilation: tag("compilation").is_some_and(|v| v == "1"),
        disc_no,
        track_no,
        year,
        duration_ms,
        codec: field(json, "codec_name"),
        bitrate: field(json, "bit_rate")
            .and_then(|v| v.parse::<u32>().ok())
            .map(|bits| bits / 1000),
        sample_rate: field(json, "sample_rate").and_then(|v| v.parse().ok()),
        // ffprobe exposes ReplayGain inconsistently across formats, and these
        // files are already the awkward ones; the enrichment pass measures them.
        rg_track_gain: None,
        rg_track_peak: None,
        rg_album_gain: None,
        rg_album_peak: None,
    })
}

/// Value of a `"name": "value"` pair, case-insensitively on the key.
///
/// Tag names vary in case between containers (`TITLE` in Vorbis, `title` in
/// ID3), so matching has to ignore it.
fn field(json: &str, name: &str) -> Option<String> {
    let needle = format!("\"{}\"", name.to_ascii_lowercase());
    let lower = json.to_ascii_lowercase();

    let mut from = 0;
    while let Some(offset) = lower[from..].find(&needle) {
        let key_start = from + offset;
        let after_key = key_start + needle.len();

        // Must be followed by a colon to be a key rather than a value.
        let rest = json.get(after_key..)?;
        let trimmed = rest.trim_start();
        if !trimmed.starts_with(':') {
            from = after_key;
            continue;
        }

        let value = trimmed[1..].trim_start();
        if let Some(text) = value.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = text.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '"' => return Some(out).filter(|s| !s.trim().is_empty()),
                    '\\' => out.push(chars.next().unwrap_or('\\')),
                    other => out.push(other),
                }
            }
            return None;
        }
        from = after_key;
    }
    None
}

fn tag_value(json: &str, name: &str) -> Option<String> {
    field(json, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "streams": [ { "codec_name": "mp3", "sample_rate": "44100", "bit_rate": "192000" } ],
        "format": {
            "duration": "86.390125",
            "tags": {
                "title": "Kick Out The Jams",
                "album": "The Presidents of the United States of America",
                "artist": "The Presidents of the United States of America",
                "track": "9/24",
                "date": "2005-08-16"
            }
        }
    }"#;

    #[test]
    fn tags_and_properties_come_out_of_ffprobe_json() {
        let meta = parse(SAMPLE, std::path::Path::new("/m/x.mp3"), 7, 99).expect("parsed");
        assert_eq!(meta.title.as_deref(), Some("Kick Out The Jams"));
        assert_eq!(meta.track_no, 9, "a 'n/total' track number is handled");
        assert_eq!(meta.year, Some(2005));
        assert_eq!(meta.duration_ms, 86_390);
        assert_eq!(meta.codec.as_deref(), Some("mp3"));
        assert_eq!(meta.sample_rate, Some(44_100));
        assert_eq!(meta.bitrate, Some(192));
        assert_eq!(meta.mtime, 7);
        assert_eq!(meta.size, 99);
    }

    #[test]
    fn uppercase_tag_names_are_matched_too() {
        let json = r#"{"format":{"duration":"10.0","tags":{"TITLE":"Shouty","ALBUM":"Caps"}}}"#;
        let meta = parse(json, std::path::Path::new("/m/x.flac"), 0, 0).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Shouty"));
        assert_eq!(meta.album_raw.as_deref(), Some("Caps"));
    }

    #[test]
    fn a_file_with_no_duration_is_not_a_track() {
        assert!(parse(r#"{"format":{}}"#, std::path::Path::new("/m/x"), 0, 0).is_none());
        assert!(
            parse(
                r#"{"format":{"duration":"0.0"}}"#,
                std::path::Path::new("/m/x"),
                0,
                0
            )
            .is_none()
        );
    }

    #[test]
    fn missing_tags_are_absent_rather_than_empty() {
        let json = r#"{"format":{"duration":"5.0","tags":{"title":"  "}}}"#;
        let meta = parse(json, std::path::Path::new("/m/x"), 0, 0).unwrap();
        assert_eq!(meta.title, None, "whitespace is not a title");
        assert_eq!(meta.artist_raw, None);
    }

    #[test]
    fn escaped_characters_in_values_survive() {
        let json = r#"{"format":{"duration":"5.0","tags":{"title":"Say \"Hello\""}}}"#;
        let meta = parse(json, std::path::Path::new("/m/x"), 0, 0).unwrap();
        assert_eq!(meta.title.as_deref(), Some(r#"Say "Hello""#));
    }

    #[test]
    fn a_key_appearing_as_a_value_is_not_mistaken_for_a_field() {
        // "album" occurs as a *value* here before appearing as a key.
        let json = r#"{"format":{"duration":"5.0","tags":{"comment":"album","album":"Real"}}}"#;
        let meta = parse(json, std::path::Path::new("/m/x"), 0, 0).unwrap();
        assert_eq!(meta.album_raw.as_deref(), Some("Real"));
    }
}
