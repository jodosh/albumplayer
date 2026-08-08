//! `albumplayer` — command-line front end for the library core.
//!
//! Phase 1 of the project: scan a music tree, inspect how it resolved into
//! albums, and audit the tagging. Playback lands in a later phase.

use std::path::PathBuf;

use albumplayer_core::query::{AlbumFilter, AlbumSort};
use albumplayer_core::{Library, scan};
#[cfg(feature = "playback")]
use albumplayer_engine as engine;
use albumplayer_enrich as enrich;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "albumplayer",
    version,
    about = "An album-first music library"
)]
struct Cli {
    /// Database location. Defaults to $XDG_DATA_HOME/albumplayer/library.db.
    #[arg(long, global = true, value_name = "FILE")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan music directories into the library.
    Scan {
        /// Directories to add and scan. Omit to rescan the known roots.
        roots: Vec<PathBuf>,
        /// Print every file that failed to parse.
        #[arg(long)]
        verbose: bool,
        /// Re-read tags from every file instead of trusting the cache.
        #[arg(long)]
        force: bool,
    },
    /// List albums.
    Albums {
        /// artist | title | year | plays | added | last
        #[arg(long, default_value = "artist")]
        sort: String,
        /// Match album title or album artist.
        #[arg(long)]
        search: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Include albums whose files are no longer on disk.
        #[arg(long)]
        include_missing: bool,
    },
    /// List artists by how much you have listened to them.
    Artists {
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Show one album's tracklist.
    Album { id: i64 },
    /// Library totals.
    Stats,
    /// Report tagging problems that will affect album-order playback.
    Doctor {
        /// Only show this category of problem.
        #[arg(long)]
        kind: Option<String>,
        /// Maximum examples to print per category.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Registered library roots.
    Roots,
    /// Record a listening session by hand. Useful for exercising the play log
    /// before the playback engine exists.
    Log {
        album_id: i64,
        /// Number of leading tracks to mark as fully played.
        #[arg(long)]
        tracks: Option<usize>,
    },
    /// Play albums. Album order is preserved; shuffle reorders albums only.
    #[cfg(feature = "playback")]
    Play {
        /// Album IDs to queue, in order. Omit with --all to queue everything.
        album_ids: Vec<i64>,
        /// Queue the whole library.
        #[arg(long)]
        all: bool,
        /// Shuffle the album order (never the tracks within an album).
        #[arg(long)]
        shuffle: bool,
        /// Loop: `album` or `queue`.
        #[arg(long)]
        repeat: Option<String>,
        /// GStreamer sink description, e.g. "pulsesink" or "fakesink sync=true".
        #[arg(long)]
        sink: Option<String>,
    },
    /// Measure album loudness so volume is even between records.
    ///
    /// Reads the audio with ffmpeg and stores the result in the library. Your
    /// music files are never modified.
    Replaygain {
        /// Re-measure albums that already have a value.
        #[arg(long)]
        force: bool,
        /// Albums analysed at once. Higher is not always faster on a network share.
        #[arg(long, default_value_t = 8)]
        jobs: usize,
        /// Stop after this many albums.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Fetch missing cover art from the Cover Art Archive.
    ///
    /// Rate limited to one MusicBrainz lookup per second, so a large library
    /// takes a while. Covers are cached, not written into your music folders.
    Artwork {
        /// Retry albums previously looked up and not found.
        #[arg(long)]
        force: bool,
        /// Stop after this many albums.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Most-played albums and artists.
    Top {
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Only count plays from the last N days.
        #[arg(long)]
        days: Option<i64>,
    },
}

fn main() -> Result<()> {
    restore_sigpipe();
    let cli = Cli::parse();
    let db_path = match cli.db {
        Some(p) => p,
        None => default_db_path()?,
    };
    let mut lib = Library::open(&db_path)
        .with_context(|| format!("opening library at {}", db_path.display()))?;

    match cli.command {
        Command::Scan {
            roots,
            verbose,
            force,
        } => cmd_scan(&mut lib, roots, verbose, force)?,
        Command::Albums {
            sort,
            search,
            limit,
            include_missing,
        } => cmd_albums(&lib, &sort, search, limit, include_missing)?,
        Command::Artists { limit } => cmd_artists(&lib, limit)?,
        Command::Album { id } => cmd_album(&lib, id)?,
        Command::Stats => cmd_stats(&lib)?,
        Command::Doctor { kind, limit } => cmd_doctor(&lib, kind, limit)?,
        Command::Roots => {
            for root in lib.roots()? {
                println!("{root}");
            }
        }
        Command::Log { album_id, tracks } => cmd_log(&lib, album_id, tracks)?,
        #[cfg(feature = "playback")]
        Command::Play {
            album_ids,
            all,
            shuffle,
            repeat,
            sink,
        } => cmd_play(&lib, &album_ids, all, shuffle, repeat.as_deref(), sink.as_deref())?,
        Command::Replaygain { force, jobs, limit } => {
            cmd_replaygain(&lib, force, jobs, limit)?
        }
        Command::Artwork { force, limit } => cmd_artwork(&lib, force, limit)?,
        Command::Top { limit, days } => cmd_top(&lib, limit, days)?,
    }
    Ok(())
}

/// Restore the default SIGPIPE behaviour.
///
/// Rust ignores SIGPIPE, which turns `albumplayer albums | head` into a panic
/// on a broken pipe instead of a clean exit. Piping into `head` is ordinary
/// use of a listing command, so the shell convention wins here.
fn restore_sigpipe() {
    // SAFETY: setting a signal disposition to the default before any threads
    // exist, which is exactly what the C library expects.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn default_db_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("neither XDG_DATA_HOME nor HOME is set; pass --db")?;
    Ok(base.join("albumplayer/library.db"))
}

fn cmd_scan(lib: &mut Library, roots: Vec<PathBuf>, verbose: bool, force: bool) -> Result<()> {
    for root in &roots {
        if !root.is_dir() {
            bail!("{} is not a directory", root.display());
        }
        lib.add_root(root)?;
    }

    let report = scan::scan_library(lib, scan::ScanOptions { force })?;

    println!(
        "Scanned {} files in {:.1}s  ({} parsed, {} unchanged, {} failed)",
        report.files_seen,
        report.duration_ms as f64 / 1000.0,
        report.files_parsed,
        report.files_cached,
        report.files_failed,
    );
    println!(
        "{} albums ({} new){}",
        report.albums,
        report.albums_new,
        if report.albums_gone > 0 {
            format!(", {} no longer on disk", report.albums_gone)
        } else {
            String::new()
        }
    );

    if report.absences_skipped {
        println!(
            "\n⚠ {} of {} files could not be read, so nothing was marked as missing.\n\
             \x20 That failure rate usually means the storage was unavailable rather than\n\
             \x20 the music being gone. Rescan once it is healthy.",
            report.files_failed, report.files_seen
        );
    }

    if report.files_failed > 0 {
        let shown = if verbose { report.errors.len() } else { 5 };
        println!("\nUnreadable files:");
        for (path, err) in report.errors.iter().take(shown) {
            println!("  {}: {err}", path.display());
        }
        if !verbose && report.errors.len() > shown {
            println!("  … and more; rerun with --verbose");
        }
    }

    println!("\nRun `albumplayer doctor` to review how these resolved into albums.");
    Ok(())
}

fn cmd_albums(
    lib: &Library,
    sort: &str,
    search: Option<String>,
    limit: i64,
    include_missing: bool,
) -> Result<()> {
    let sort = AlbumSort::parse(sort)
        .with_context(|| format!("unknown sort '{sort}' (artist|title|year|plays|added|last)"))?;
    let albums = lib.albums(
        sort,
        &AlbumFilter {
            search,
            include_missing,
            limit: Some(limit),
        },
    )?;

    if albums.is_empty() {
        println!("No albums. Run `albumplayer scan <DIR>` first.");
        return Ok(());
    }

    for a in &albums {
        let year = a.year.map(|y| y.to_string()).unwrap_or_else(|| "----".into());
        let discs = if a.disc_count > 1 {
            format!(" [{}CD]", a.disc_count)
        } else {
            String::new()
        };
        let plays = if a.play_count > 0 {
            format!("  ▸{}", a.play_count)
        } else {
            String::new()
        };
        println!(
            "{:>6}  {year}  {:<28.28}  {:<40.40}  {:>2}tr {:>6}{discs}{plays}",
            a.id,
            a.album_artist,
            a.title,
            a.track_count,
            fmt_duration(a.duration_ms),
        );
    }
    println!("\n{} albums", albums.len());
    Ok(())
}

fn cmd_artists(lib: &Library, limit: i64) -> Result<()> {
    let artists = lib.artists(Some(limit))?;
    if artists.is_empty() {
        println!("No artists yet.");
        return Ok(());
    }
    println!("{:<40} {:>7} {:>12} {:>12}", "ARTIST", "ALBUMS", "ALBUM PLAYS", "TRACK PLAYS");
    for a in &artists {
        println!(
            "{:<40.40} {:>7} {:>12} {:>12}",
            a.name, a.album_count, a.album_plays, a.track_plays
        );
    }
    Ok(())
}

fn cmd_album(lib: &Library, id: i64) -> Result<()> {
    let album = lib.album(id)?;
    let tracks = lib.album_tracks(id)?;

    let year = album.year.map(|y| format!(" ({y})")).unwrap_or_default();
    println!("{} — {}{year}", album.album_artist, album.title);
    println!("{}", album.dir_path);
    if let Some(art) = lib.album_art(id)? {
        let cache = enrich::artwork::default_cache_dir()?;
        println!("cover: {}", art.resolve(&cache).display());
    }
    if let (Some(gain), peak) = lib.album_replaygain(id)? {
        let headroom = peak
            .map(|p| format!(", peak {p:.3}"))
            .unwrap_or_default();
        println!("replaygain: {gain:+.1} dB{headroom}");
    }
    println!(
        "{} tracks, {}{}{}\n",
        album.track_count,
        fmt_duration(album.duration_ms),
        if album.disc_count > 1 {
            format!(", {} discs", album.disc_count)
        } else {
            String::new()
        },
        if album.is_compilation {
            ", compilation"
        } else {
            ""
        },
    );

    let mut current_disc = 0;
    for t in &tracks {
        if album.disc_count > 1 && t.disc_no != current_disc {
            current_disc = t.disc_no;
            println!("  Disc {current_disc}");
        }
        println!(
            "  {:>3}. {:<44.44} {:>7}  {}",
            t.track_no,
            t.title,
            fmt_duration(t.duration_ms),
            t.artist,
        );
    }
    Ok(())
}

fn cmd_stats(lib: &Library) -> Result<()> {
    let s = lib.stats()?;
    println!("Albums         {}", s.albums);
    println!("  compilations {}", s.compilations);
    println!("Artists        {}", s.artists);
    println!("Tracks         {}", s.tracks);
    println!("Total time     {}", fmt_duration(s.total_duration_ms));
    println!("Album plays    {}", s.album_plays);
    println!("Track plays    {}", s.track_plays);

    let coverage = |sql: &str| -> Result<i64> {
        Ok(lib.conn.query_row(sql, [], |r| r.get(0))?)
    };
    let with_art = coverage(
        "SELECT COUNT(*) FROM album
         WHERE present = 1 AND (art_path IS NOT NULL OR art_cache_path IS NOT NULL)",
    )?;
    let with_gain = coverage(
        "SELECT COUNT(*) FROM album al
         WHERE al.present = 1 AND (al.rg_gain_db IS NOT NULL
           OR EXISTS (SELECT 1 FROM track t
                       WHERE t.album_id = al.id AND t.rg_album_gain IS NOT NULL))",
    )?;
    println!("\nCover art      {with_art} of {} albums", s.albums);
    println!("ReplayGain     {with_gain} of {} albums", s.albums);
    if s.missing_albums > 0 || s.missing_tracks > 0 {
        println!(
            "\nNo longer on disk: {} albums, {} tracks (history retained)",
            s.missing_albums, s.missing_tracks
        );
    }
    Ok(())
}

fn cmd_doctor(lib: &Library, kind: Option<String>, limit: usize) -> Result<()> {
    let anomalies = lib.doctor()?;
    if anomalies.is_empty() {
        println!("No tagging problems found.");
        return Ok(());
    }

    // Preserve the order doctor() emits so related categories stay together.
    let mut categories: Vec<&'static str> = Vec::new();
    for a in &anomalies {
        if !categories.contains(&a.kind) {
            categories.push(a.kind);
        }
    }

    for category in categories {
        if let Some(filter) = &kind
            && filter != category
        {
            continue;
        }
        let items: Vec<_> = anomalies.iter().filter(|a| a.kind == category).collect();
        println!("\n{}  ({})", category, items.len());
        println!("{}", explain(category));
        for a in items.iter().take(limit) {
            println!("  {}", a.detail);
        }
        if items.len() > limit {
            println!("  … {} more", items.len() - limit);
        }
    }
    Ok(())
}

/// Why each anomaly category matters for album-first playback.
fn explain(kind: &str) -> &'static str {
    match kind {
        "no-album-tag" => "  identity rests on the folder path; renaming it will orphan play history",
        "unknown-artist" => "  no artist tag and no usable fallback",
        "missing-track-numbers" => "  tracks will play in filename order, not album order",
        "duplicate-track-numbers" => "  album order is ambiguous",
        "single-track-album" => "  likely a stray single, or a grouping failure worth checking",
        "no-cover-art" => "  no cover image found in the album folder",
        "no-album-replaygain" => "  album gain missing; volume will jump between albums",
        "merged-from-several-folders" => "  separate folders resolved to one release and were merged; check they belong together",
        "mixed-codecs" => "  one album spans several formats, which can disturb gapless playback",
        _ => "",
    }
}

fn cmd_log(lib: &Library, album_id: i64, tracks: Option<usize>) -> Result<()> {
    let all = lib.album_tracks(album_id)?;
    if all.is_empty() {
        bail!("album {album_id} has no tracks on disk");
    }
    let count = tracks.unwrap_or(all.len()).min(all.len());

    let session = lib.start_album_session(album_id)?;
    let started = albumplayer_core::util::now_unix();
    for t in all.iter().take(count) {
        lib.record_play(Some(session), t.id, started, t.duration_ms)?;
    }
    let finished = lib.end_album_session(session)?;

    println!(
        "Logged {count} of {} tracks — album play {}",
        all.len(),
        if finished { "counted" } else { "not counted" }
    );
    Ok(())
}

#[cfg(feature = "playback")]
/// Play albums, logging what gets heard.
///
/// Two threads: this one reads keyboard commands, while a second drains the
/// engine's events, prints them, and writes them to the play log.
fn cmd_play(
    lib: &Library,
    album_ids: &[i64],
    all: bool,
    shuffle: bool,
    repeat: Option<&str>,
    sink: Option<&str>,
) -> Result<()> {
    let mut ids: Vec<i64> = if all {
        lib.albums(AlbumSort::Artist, &AlbumFilter::default())?
            .into_iter()
            .map(|a| a.id)
            .collect()
    } else {
        album_ids.to_vec()
    };
    if ids.is_empty() {
        bail!("nothing to play: give album IDs or --all");
    }
    // Shuffle the order up front as well as telling the engine. The engine
    // pins whichever album is playing when shuffle is toggled mid-listen, so
    // without this the first album would always be the same one.
    if shuffle {
        shuffle_in_place(&mut ids);
    }

    // Load everything up front so a database error surfaces before any audio.
    let mut albums = Vec::with_capacity(ids.len());
    for id in &ids {
        let album = lib.album(*id)?;
        let tracks = lib.album_tracks(*id)?;
        if tracks.is_empty() {
            eprintln!("skipping {}: no playable tracks", album.title);
            continue;
        }
        let (gain, peak) = lib.album_replaygain(*id)?;
        albums.push(engine::album_from_library(&album, &tracks, gain, peak));
    }
    if albums.is_empty() {
        bail!("none of those albums have playable tracks");
    }

    let (player, events) = match sink {
        Some(desc) => engine::Player::with_sink(desc),
        None => engine::Player::new(),
    }
    .context("starting the playback engine")?;

    if shuffle {
        player.set_shuffle(true)?;
    }
    match repeat {
        Some("album") => player.set_repeat(engine::Repeat::Album)?,
        Some("queue") => player.set_repeat(engine::Repeat::Queue)?,
        Some(other) => bail!("unknown repeat mode '{other}' (album|queue)"),
        None => {}
    }

    let mut albums = albums.into_iter();
    if let Some(first) = albums.next() {
        player.play_album(first)?;
    }
    for album in albums {
        player.enqueue(album)?;
    }

    println!("{}\n", CONTROLS);

    // Keyboard control runs on its own thread with a Send handle, while events
    // and the play log stay here — the database connection cannot leave this
    // thread.
    let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let control = player.control();
    let key_quit = std::sync::Arc::clone(&quit);
    let keys = std::thread::spawn(move || {
        let result = read_controls(&control, &key_quit);
        if let Err(e) = result {
            eprintln!("controls: {e}");
        }
    });

    let mut logger = engine::PlayLogger::new(lib);
    loop {
        // Poll rather than block, so quitting from the keyboard is noticed even
        // though no further events will arrive after a stop.
        match events.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(event) => {
                report(&event);
                logger.handle(&event);
                if matches!(event, engine::PlayerEvent::QueueFinished) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if quit.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
    }
    // Settle the session before reporting, so `finished` is accurate.
    logger.close_session();
    for problem in logger.errors() {
        eprintln!("play log: {problem}");
    }

    // The engine stops when `player` drops, which unblocks nothing on stdin —
    // so the key thread is left detached rather than joined. It exits with the
    // process.
    drop(player);
    let _ = keys;

    Ok(())
}

#[cfg(feature = "playback")]
/// Fisher-Yates with a clock seed, so `--shuffle` differs between runs.
fn shuffle_in_place(ids: &mut [i64]) {
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..ids.len()).rev() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        ids.swap(i, (state.wrapping_mul(0x2545_F491_4F6C_DD1D) as usize) % (i + 1));
    }
}

#[cfg(feature = "playback")]
const CONTROLS: &str = "\
[enter] next track   [a] next album   [b] previous track   [B] previous album
[p] pause/resume     [s] shuffle      [q] quit";

#[cfg(feature = "playback")]
/// Print a playback event in a human-readable line.
fn report(event: &engine::PlayerEvent) {
    use engine::PlayerEvent as E;
    match event {
        E::AlbumStarted { title, .. } => println!("\n▶ {title}"),
        E::TrackStarted { title, .. } => println!("   {title}"),
        E::QueueFinished => println!("\nQueue finished."),
        E::Error(message) => eprintln!("   ! {message}"),
        E::TrackFinished { .. } | E::StateChanged(_) => {}
    }
}

#[cfg(feature = "playback")]
/// Read single-word commands from stdin until the user quits.
///
/// Reaching end of input is not a quit: `albumplayer play 42 < /dev/null`
/// should play the album through, just without interactive control.
fn read_controls(
    player: &engine::PlayerControl,
    quit: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::io::BufRead;

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(()); // input closed; let playback continue
        }
        match line.trim() {
            "" | "n" => player.next_track()?,
            "a" => player.next_album()?,
            "b" => player.prev_track()?,
            "B" => player.prev_album()?,
            "p" => player.toggle_pause()?,
            "s" => {
                let on = !player.status().shuffle;
                player.set_shuffle(on)?;
                println!("   shuffle {}", if on { "on" } else { "off" });
            }
            "q" => break,
            other => println!("   unknown command '{other}'\n{CONTROLS}"),
        }
    }
    quit.store(true, std::sync::atomic::Ordering::SeqCst);
    player.stop()?;
    Ok(())
}

fn cmd_replaygain(lib: &Library, force: bool, jobs: usize, limit: Option<usize>) -> Result<()> {
    let options = enrich::replaygain::Options { force, jobs, limit };
    println!("Measuring album loudness with {jobs} parallel jobs. This reads every");
    println!("file, so a large library on a network share takes a while.\n");

    let report = enrich::replaygain::run_with_progress(lib, options, |done, total, album| {
        // Overwrite one line rather than scrolling a wall of album names.
        eprint!("\r  [{done}/{total}] {album:<58.58}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    })?;
    eprintln!();

    println!(
        "{} albums considered, {} measured, {} silent, {} failed in {:.0}s",
        report.considered,
        report.measured,
        report.silent,
        report.failed,
        report.duration_ms as f64 / 1000.0,
    );
    for (album, problem) in report.errors.iter().take(10) {
        println!("  {album}: {problem}");
    }
    Ok(())
}

fn cmd_artwork(lib: &Library, force: bool, limit: Option<usize>) -> Result<()> {
    let options = enrich::artwork::Options {
        force,
        limit,
        cache_dir: None,
    };
    let cache = enrich::artwork::default_cache_dir()?;
    println!("Fetching covers into {}", cache.display());
    println!("MusicBrainz allows one lookup per second, so this is slow by design.\n");

    let report = enrich::artwork::run(lib, &options)?;

    println!(
        "{} albums considered, {} covers fetched, {} not found, {} failed in {:.0}s",
        report.considered,
        report.fetched,
        report.not_found,
        report.failed,
        report.duration_ms as f64 / 1000.0,
    );
    for (album, problem) in report.errors.iter().take(10) {
        println!("  {album}: {problem}");
    }
    Ok(())
}

fn cmd_top(lib: &Library, limit: i64, days: Option<i64>) -> Result<()> {
    let since = days.map(|d| albumplayer_core::util::now_unix() - d * 86_400);
    let window = days
        .map(|d| format!(" (last {d} days)"))
        .unwrap_or_default();

    let albums = lib.top_albums(limit, since)?;
    println!("Top albums{window}");
    if albums.is_empty() {
        println!("  nothing played yet");
    }
    for (artist, title, plays) in &albums {
        println!("  {plays:>4}  {artist} — {title}");
    }

    let artists = lib.top_artists(limit, since)?;
    println!("\nTop artists{window}");
    if artists.is_empty() {
        println!("  nothing played yet");
    }
    for (name, album_plays, track_plays) in &artists {
        println!("  {album_plays:>4} albums, {track_plays:>5} tracks  {name}");
    }
    Ok(())
}

/// Render a millisecond duration as `h:mm:ss` or `m:ss`.
fn fmt_duration(ms: i64) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_render_readably() {
        assert_eq!(fmt_duration(0), "0:00");
        assert_eq!(fmt_duration(45_000), "0:45");
        assert_eq!(fmt_duration(213_000), "3:33");
        assert_eq!(fmt_duration(3_723_000), "1:02:03");
    }

    #[test]
    fn cli_parses_its_own_definition() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
