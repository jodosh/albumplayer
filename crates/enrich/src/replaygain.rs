//! Measuring album loudness so playback volume is even between records.
//!
//! Most of a real library has no ReplayGain tags at all, which is exactly the
//! problem an album-first player cannot ignore: without it, every change of
//! album is a jump in volume.
//!
//! Loudness is measured with ffmpeg's `ebur128` filter over the album's tracks
//! **concatenated into one stream**, because ReplayGain album gain is defined
//! as the loudness of the whole record played end to end — not the average of
//! its tracks. Results go in the database rather than into the audio files;
//! nothing here writes to the user's music.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use albumplayer_core::Library;
use albumplayer_core::util::now_unix;
use rayon::prelude::*;
use rusqlite::params;

use crate::{Error, Result};

/// ReplayGain 2.0 reference level. Gain is the correction that would bring a
/// recording to this loudness.
const REFERENCE_LUFS: f64 = -18.0;

/// Loudness reported for digital silence. Anything at or below this is a track
/// with no signal, and normalizing it would be meaningless.
const SILENCE_LUFS: f64 = -70.0;

/// Gains beyond this are almost certainly a measurement failure rather than a
/// genuinely quiet record, and applying one would be unpleasant.
const MAX_ABS_GAIN_DB: f64 = 30.0;

/// The loudness of one album.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Integrated loudness over the whole album, in LUFS.
    pub loudness_lufs: f64,
    /// Correction needed to reach the reference level, in dB.
    pub gain_db: f64,
    /// True peak as a linear sample value, where 1.0 is full scale.
    pub peak: f64,
}

/// Convert an integrated loudness reading into a ReplayGain measurement.
pub fn measurement_from(loudness_lufs: f64, true_peak_dbfs: f64) -> Measurement {
    Measurement {
        loudness_lufs,
        gain_db: (REFERENCE_LUFS - loudness_lufs).clamp(-MAX_ABS_GAIN_DB, MAX_ABS_GAIN_DB),
        peak: 10f64.powf(true_peak_dbfs / 20.0),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Re-measure albums that already have a stored measurement.
    pub force: bool,
    /// Albums analysed at once. The library usually lives on a network share,
    /// where too much parallelism slows everything down rather than speeding it
    /// up, so this stays modest by default.
    pub jobs: usize,
    /// Stop after this many albums. Useful for a trial run.
    pub limit: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            force: false,
            jobs: 8,
            limit: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub considered: usize,
    pub measured: usize,
    pub failed: usize,
    pub silent: usize,
    pub errors: Vec<(String, String)>,
    pub duration_ms: u128,
}

/// An album awaiting measurement.
struct Pending {
    id: i64,
    label: String,
    tracks: Vec<PathBuf>,
}

/// Measure every album that needs it and store the results.
///
/// Each measurement is written as soon as it lands rather than at the end. A
/// full library takes the better part of an hour, and batching the writes would
/// mean an interrupted run threw all of that away.
pub fn run(library: &Library, options: Options) -> Result<Report> {
    run_with_progress(library, options, |_, _, _| {})
}

/// As [`run`], reporting each album as it completes: `(done, total, label)`.
pub fn run_with_progress(
    library: &Library,
    options: Options,
    progress: impl Fn(usize, usize, &str) + Sync,
) -> Result<Report> {
    let started = std::time::Instant::now();
    let pending = collect_pending(library, options)?;
    let total = pending.len();

    let mut report = Report {
        considered: total,
        ..Default::default()
    };

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs.max(1))
        .build()
        .map_err(|e| Error::Other(e.to_string()))?;

    // Workers decode in parallel and hand results back over a channel; this
    // thread owns the database connection and does every write.
    let (sender, receiver) = std::sync::mpsc::channel();

    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(|| {
            pool.install(|| {
                pending.par_iter().for_each_with(sender, |sender, album| {
                    let outcome = analyze(&album.tracks).map_err(|e| e.to_string());
                    let _ = sender.send((album.id, album.label.clone(), outcome));
                });
            });
            // `for_each_with` drops the last clone here, ending the receive loop.
        });

        let mut done = 0;
        for (album_id, label, outcome) in receiver {
            done += 1;
            match outcome {
                Ok(measurement) if measurement.loudness_lufs <= SILENCE_LUFS => {
                    report.silent += 1;
                    if report.errors.len() < 50 {
                        report
                            .errors
                            .push((label.clone(), "no measurable audio (silent)".into()));
                    }
                }
                Ok(measurement) => {
                    store(library, album_id, &measurement)?;
                    report.measured += 1;
                }
                Err(message) => {
                    report.failed += 1;
                    if report.errors.len() < 50 {
                        report.errors.push((label.clone(), message));
                    }
                }
            }
            progress(done, total, &label);
        }
        Ok(())
    })?;

    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

fn collect_pending(library: &Library, options: Options) -> Result<Vec<Pending>> {
    // Albums that already carry ReplayGain tags are left alone unless forced:
    // whoever tagged them may have used a different reference, and overriding
    // existing metadata is not this tool's job.
    let sql = if options.force {
        "SELECT al.id, ar.name || ' — ' || al.title
         FROM album al JOIN artist ar ON ar.id = al.album_artist_id
         WHERE al.present = 1
         ORDER BY ar.sort_name, al.sort_title"
    } else {
        "SELECT al.id, ar.name || ' — ' || al.title
         FROM album al JOIN artist ar ON ar.id = al.album_artist_id
         WHERE al.present = 1
           AND al.rg_gain_db IS NULL
           AND NOT EXISTS (SELECT 1 FROM track t
                            WHERE t.album_id = al.id AND t.rg_album_gain IS NOT NULL)
         ORDER BY ar.sort_name, al.sort_title"
    };

    let mut stmt = library.conn.prepare(sql)?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut pending = Vec::new();
    for (id, label) in rows {
        if options.limit.is_some_and(|n| pending.len() >= n) {
            break;
        }
        let tracks: Vec<PathBuf> = library
            .album_tracks(id)?
            .into_iter()
            .map(|t| PathBuf::from(t.path))
            .collect();
        if !tracks.is_empty() {
            pending.push(Pending { id, label, tracks });
        }
    }
    Ok(pending)
}

fn store(library: &Library, album_id: i64, measurement: &Measurement) -> Result<()> {
    library.conn.execute(
        "UPDATE album SET rg_gain_db = ?2, rg_peak = ?3, rg_measured_at = ?4 WHERE id = ?1",
        params![album_id, measurement.gain_db, measurement.peak, now_unix()],
    )?;
    Ok(())
}

/// Measure one album by decoding its tracks as a single continuous stream.
pub fn analyze(tracks: &[PathBuf]) -> Result<Measurement> {
    if tracks.is_empty() {
        return Err(Error::Other("album has no tracks".into()));
    }

    // The concat demuxer needs a seekable playlist, so this goes through a
    // temporary file rather than a pipe — it rejects `pipe:0` outright.
    let playlist = PlaylistFile::write(&concat_playlist(tracks))?;

    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-f",
            "concat",
            "-safe",
            "0",
            "-protocol_whitelist",
            "file",
            "-i",
        ])
        .arg(playlist.path())
        .args([
            // Resampling to a fixed format first stops the filter graph from
            // reinitialising between tracks of differing rates, which would
            // otherwise restart the measurement partway through the album.
            "-filter_complex",
            "aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,ebur128=peak=true",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::Other(format!("running ffmpeg: {e}")))?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_ebur128(&stderr).ok_or_else(|| {
        let tail: String = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join(" / ");
        Error::Other(format!("no loudness in ffmpeg output: {tail}"))
    })
}

/// A temporary concat playlist, removed when it goes out of scope.
struct PlaylistFile(PathBuf);

impl PlaylistFile {
    fn write(contents: &str) -> Result<Self> {
        // Albums are measured in parallel, so the name has to be unique per
        // call rather than per process.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "albumplayer-rg-{}-{}.txt",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PlaylistFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Build a concat-demuxer playlist. Single quotes in paths are escaped the way
/// the demuxer expects, so filenames with apostrophes work.
fn concat_playlist(tracks: &[PathBuf]) -> String {
    tracks
        .iter()
        .map(|p| format!("file '{}'\n", p.to_string_lossy().replace('\'', r"'\''")))
        .collect()
}

/// Pull the final integrated loudness and true peak out of ffmpeg's output.
///
/// The last progress line carries the running totals for everything decoded so
/// far, which for a completed run is the whole album. Reading that rather than
/// a summary block sidesteps the extra summaries ffmpeg emits whenever the
/// filter graph reconfigures mid-stream.
fn parse_ebur128(stderr: &str) -> Option<Measurement> {
    let mut loudness = None;
    let mut peak_dbfs = None;

    for line in stderr.lines() {
        if !line.contains("I:") || !line.contains("TPK:") {
            continue;
        }
        if let Some(value) = field_after(line, "I:") {
            loudness = Some(value);
        }
        // TPK reports one figure per channel; the loudest governs headroom.
        //
        // The line also carries FTPK (the per-frame peak), which *contains*
        // "TPK:" — matching that one instead yields the current frame, often
        // `-inf` during a silent passage. Taking the last occurrence gets the
        // running true peak.
        if let Some((_, rest)) = line.rsplit_once("TPK:") {
            let loudest = rest
                .split_whitespace()
                .take_while(|t| *t != "dBFS")
                .filter_map(|t| t.parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))));
            if let Some(max) = loudest {
                peak_dbfs = Some(max);
            }
        }
    }

    Some(measurement_from(loudness?, peak_dbfs.unwrap_or(0.0)))
}

/// Read the number following `label` in an ffmpeg status line.
fn field_after(line: &str, label: &str) -> Option<f64> {
    line.split(label)
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_is_the_distance_to_the_reference_level() {
        // A record measured at -10.2 LUFS is 7.8 dB too loud.
        let m = measurement_from(-10.2, 0.9);
        assert!((m.gain_db - -7.8).abs() < 0.001, "{}", m.gain_db);
    }

    #[test]
    fn a_quiet_record_gets_positive_gain() {
        let m = measurement_from(-24.0, -3.0);
        assert!((m.gain_db - 6.0).abs() < 0.001);
    }

    #[test]
    fn true_peak_becomes_a_linear_value() {
        // 0 dBFS is full scale; above it the sample peak exceeds 1.0.
        assert!((measurement_from(-18.0, 0.0).peak - 1.0).abs() < 0.001);
        assert!(measurement_from(-18.0, 0.9).peak > 1.0);
        assert!(measurement_from(-18.0, -6.0).peak < 0.51);
    }

    #[test]
    fn absurd_gains_are_clamped() {
        let m = measurement_from(-200.0, -60.0);
        assert_eq!(m.gain_db, MAX_ABS_GAIN_DB);
    }

    #[test]
    fn the_last_running_total_is_the_album_measurement() {
        // Two progress lines: the later one covers the whole album and must win.
        let stderr = "\
[Parsed_ebur128_2 @ 0x1] t: 100.0 TARGET:-23 LUFS M:-20.0 S:-20.0 I: -14.0 LUFS LRA: 5.0 LU FTPK: -1.0 -1.0 dBFS TPK: -1.0 -1.0 dBFS
[Parsed_ebur128_2 @ 0x1] t: 2999.8 TARGET:-23 LUFS M:-18.1 S:-43.5 I: -10.2 LUFS LRA: 7.1 LU FTPK: -inf -inf dBFS TPK:   0.7   0.9 dBFS
";
        let m = parse_ebur128(stderr).expect("parsed");
        assert!((m.loudness_lufs - -10.2).abs() < 0.001);
        // The louder of the two channels sets the peak.
        assert!((m.peak - 10f64.powf(0.9 / 20.0)).abs() < 0.001);
    }

    #[test]
    fn the_per_frame_peak_field_is_not_mistaken_for_the_true_peak() {
        // FTPK contains the substring "TPK:" and often reads -inf; picking it
        // up would silently report a peak of zero.
        let line = "[Parsed_ebur128_2 @ 0x1] t: 10.0 I: -12.0 LUFS LRA: 3.0 LU                     FTPK:  -inf  -inf dBFS TPK:  -2.0  -1.5 dBFS\n";
        let m = parse_ebur128(line).expect("parsed");
        assert!(m.peak > 0.8, "peak came out as {}", m.peak);
        assert!((m.peak - 10f64.powf(-1.5 / 20.0)).abs() < 0.001);
    }

    #[test]
    fn output_without_a_measurement_is_rejected() {
        assert!(parse_ebur128("").is_none());
        assert!(parse_ebur128("ffmpeg: no such file").is_none());
    }

    #[test]
    fn playlist_escapes_apostrophes() {
        // Filenames with apostrophes are common and would otherwise terminate
        // the quoted path early.
        let list = concat_playlist(&[PathBuf::from("/m/Guns n' Roses/01.mp3")]);
        assert_eq!(list, "file '/m/Guns n'\\'' Roses/01.mp3'\n");
        assert!(list.ends_with("\n"));
    }

    #[test]
    fn playlist_lists_every_track_in_order() {
        let list = concat_playlist(&[PathBuf::from("/a/1.mp3"), PathBuf::from("/a/2.mp3")]);
        assert_eq!(list, "file '/a/1.mp3'\nfile '/a/2.mp3'\n");
    }

    #[test]
    fn an_empty_album_is_an_error() {
        assert!(analyze(&[]).is_err());
    }

    #[test]
    fn playlist_files_are_unique_and_cleaned_up() {
        let (a, b) = (
            PlaylistFile::write("file '/a.mp3'\n").unwrap(),
            PlaylistFile::write("file '/b.mp3'\n").unwrap(),
        );
        assert_ne!(a.path(), b.path(), "parallel jobs must not collide");
        assert!(a.path().exists() && b.path().exists());

        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists(), "the temporary playlist was removed");
    }
}
