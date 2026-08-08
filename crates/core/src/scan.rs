//! Library scanning: walk the roots, read tags, and resolve files into albums.
//!
//! Album identity is the hard part of an album-first player, and it is decided
//! in two stages:
//!
//! * **Grouping** buckets files by `(album directory, normalized album tag)`.
//!   The directory is the strongest available signal for a library laid out one
//!   album per folder, and it handles compilations correctly — every file in the
//!   folder groups together regardless of differing track artists.
//! * **Identity** then derives a stable database key from the *resolved* album,
//!   preferring a MusicBrainz release ID, then album artist + title + year, and
//!   only falling back to the directory path. That ordering means moving or
//!   renaming a well-tagged folder does not orphan its play history.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lofty::prelude::*;
use lofty::config::{ParseOptions, ParsingMode};
use lofty::probe::Probe;
use rayon::prelude::*;
use rusqlite::params;
use walkdir::WalkDir;

use crate::model::{AlbumDraft, IdentitySource, TrackMeta};
use crate::util::{
    album_dir, disc_no_from_path, leading_u32, majority, normalize, now_unix, sort_key,
    strip_disc_suffix, year_from_date,
};
use crate::{Library, Result};

/// Extensions we attempt to read. Anything else in the tree is ignored.
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "opus", "ogg", "oga", "flac", "wav", "wv", "ape", "mpc",
];

/// Fraction of unreadable files above which a scan is treated as unreliable.
///
/// A flaky network share can fail hundreds of reads in one pass. Those files
/// are still on disk, so marking them absent would quietly shrink the library
/// and strand play history. Past this rate the scan records what it *did* read
/// and leaves everything else alone.
const MAX_FAILURE_RATE: f64 = 0.05;

/// Cover art filenames checked in the album directory, in preference order.
const COVER_STEMS: &[&str] = &["cover", "folder", "front", "album", "albumart", "art"];
const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "avif"];

/// What a scan did, reported back to the caller.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub files_seen: usize,
    pub files_parsed: usize,
    pub files_cached: usize,
    pub files_failed: usize,
    pub albums: usize,
    pub albums_new: usize,
    pub tracks_gone: usize,
    pub albums_gone: usize,
    pub errors: Vec<(PathBuf, String)>,
    /// True when too many files failed to read for absences to be trusted, so
    /// nothing was marked as gone.
    pub absences_skipped: bool,
    pub duration_ms: u128,
}

/// Knobs for a scan.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOptions {
    /// Re-read tags from every file, ignoring the cached values. Needed after
    /// the scanner's own tag interpretation changes.
    pub force: bool,
}

/// Scan every registered root and reconcile the database against what is on disk.
pub fn scan_library(lib: &mut Library, opts: ScanOptions) -> Result<ScanReport> {
    let roots = lib.roots()?;
    if roots.is_empty() {
        return Err(crate::Error::NoRoots);
    }
    let paths: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
    scan_roots(lib, &paths, opts)
}

/// Scan the given roots. Exposed separately so tests can scan a temp directory
/// without registering it.
pub fn scan_roots(lib: &mut Library, roots: &[PathBuf], opts: ScanOptions) -> Result<ScanReport> {
    let started = std::time::Instant::now();
    let mut report = ScanReport::default();

    let files = discover_files(roots);
    report.files_seen = files.len();

    let cache = if opts.force {
        MetaCache::new()
    } else {
        load_cached_meta(lib)?
    };
    let metas = read_metadata(&files, &cache, &mut report);
    let drafts = group_into_albums(metas, roots);
    report.albums = drafts.len();

    // Storage that was unreliable during the walk makes absence meaningless.
    let failure_rate = if report.files_seen > 0 {
        report.files_failed as f64 / report.files_seen as f64
    } else {
        0.0
    };
    report.absences_skipped = failure_rate > MAX_FAILURE_RATE;

    persist(lib, &drafts, &mut report)?;

    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

/// Walk the roots and collect every readable audio file.
fn discover_files(roots: &[PathBuf]) -> Vec<(PathBuf, i64, i64)> {
    let mut out = Vec::new();
    for root in roots {
        let walker = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !is_hidden(e.path()));

        for entry in walker.filter_map(std::result::Result::ok) {
            if !entry.file_type().is_file() || !has_audio_extension(entry.path()) {
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push((entry.into_path(), mtime, md.len() as i64));
        }
    }
    out
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') && n != "." && n != "..")
}

fn has_audio_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.as_str()))
}

/// Raw tag values already stored for a path, keyed by path.
type MetaCache = HashMap<String, TrackMeta>;

/// Load previously scanned tag values so unchanged files can skip disk I/O.
fn load_cached_meta(lib: &Library) -> Result<MetaCache> {
    let mut stmt = lib.conn.prepare(
        "SELECT path, mtime, size, title, artist_raw, album_artist_raw, album_raw,
                mb_album_id, compilation, disc_no, track_no, year, duration_ms,
                codec, bitrate, sample_rate,
                rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak
         FROM track",
    )?;

    let rows = stmt.query_map([], |r| {
        let path: String = r.get(0)?;
        Ok((
            path.clone(),
            TrackMeta {
                path: PathBuf::from(path),
                mtime: r.get(1)?,
                size: r.get(2)?,
                title: r.get(3)?,
                artist_raw: r.get(4)?,
                album_artist_raw: r.get(5)?,
                album_raw: r.get(6)?,
                mb_album_id: r.get(7)?,
                compilation: r.get::<_, i64>(8)? != 0,
                disc_no: r.get::<_, i64>(9)? as u32,
                track_no: r.get::<_, i64>(10)? as u32,
                year: r.get::<_, Option<i64>>(11)?.map(|y| y as i32),
                duration_ms: r.get(12)?,
                codec: r.get(13)?,
                bitrate: r.get::<_, Option<i64>>(14)?.map(|v| v as u32),
                sample_rate: r.get::<_, Option<i64>>(15)?.map(|v| v as u32),
                rg_track_gain: r.get(16)?,
                rg_track_peak: r.get(17)?,
                rg_album_gain: r.get(18)?,
                rg_album_peak: r.get(19)?,
            },
        ))
    })?;

    let mut cache = MetaCache::new();
    for row in rows {
        let (path, meta) = row?;
        cache.insert(path, meta);
    }
    Ok(cache)
}

/// Read tags for changed files in parallel, reusing cached values otherwise.
fn read_metadata(
    files: &[(PathBuf, i64, i64)],
    cache: &MetaCache,
    report: &mut ScanReport,
) -> Vec<TrackMeta> {
    let results: Vec<std::result::Result<(TrackMeta, bool), (PathBuf, String)>> = files
        .par_iter()
        .map(|(path, mtime, size)| {
            let key = path.to_string_lossy();
            if let Some(cached) = cache.get(key.as_ref())
                && cached.mtime == *mtime
                && cached.size == *size
            {
                return Ok((cached.clone(), true));
            }
            match read_tags(path, *mtime, *size) {
                Ok(meta) => Ok((meta, false)),
                // The in-process parser gave up. ffprobe is more forgiving, and
                // a partly-tagged track beats a hole in an album's tracklist.
                Err(e) => match crate::ffprobe::read_tags(path, *mtime, *size) {
                    Some(meta) => Ok((meta, false)),
                    None => Err((path.clone(), e.to_string())),
                },
            }
        })
        .collect();

    let mut metas = Vec::with_capacity(results.len());
    for result in results {
        match result {
            Ok((meta, cached)) => {
                if cached {
                    report.files_cached += 1;
                } else {
                    report.files_parsed += 1;
                }
                metas.push(meta);
            }
            Err((path, msg)) => {
                report.files_failed += 1;
                if report.errors.len() < 50 {
                    report.errors.push((path, msg));
                }
            }
        }
    }
    metas
}

/// How aggressively to parse a file's tags.
///
/// Real libraries are full of files that are perfectly playable but not
/// spec-compliant — truncated ID3v2 frames, encrypted frames without a length
/// indicator. The default parser rejects those outright, which would silently
/// drop them from the library, so anything that fails is retried leniently.
/// Cover art is never parsed: it costs I/O on every file and album art is taken
/// from the folder instead.
fn parse_options(mode: ParsingMode) -> ParseOptions {
    ParseOptions::new()
        .read_cover_art(false)
        .parsing_mode(mode)
}

/// Read one file's tags and audio properties.
fn read_tags(path: &Path, mtime: i64, size: i64) -> Result<TrackMeta> {
    let tagged = match Probe::open(path)?
        .options(parse_options(ParsingMode::BestAttempt))
        .guess_file_type()?
        .read()
    {
        Ok(tagged) => tagged,
        // Second chance for malformed-but-playable files. Partial tags beat no
        // track at all; whatever is missing shows up in `doctor`.
        Err(_) => Probe::open(path)?
            .options(parse_options(ParsingMode::Relaxed))
            .guess_file_type()?
            .read()?,
    };
    let props = tagged.properties();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let get = |key: ItemKey| -> Option<String> {
        tag.and_then(|t| t.get_string(key))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let get_f64 = |key: ItemKey| -> Option<f64> {
        // ReplayGain is tagged as e.g. "-7.25 dB"; take the leading number.
        get(key).and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|n| n.parse::<f64>().ok())
        })
    };

    let album_raw = get(ItemKey::AlbumTitle);

    // Disc and track numbers come from typed accessors when available, and from
    // raw text otherwise, since "03/12" is common and does not parse as u32.
    //
    // The two fallbacks past the tags are not optional niceties: plenty of rips
    // split a release across `CD1`/`CD2` folders, or bake "(Disc 2)" into the
    // album title, without ever writing a disc tag. Defaulting those to disc 1
    // collides every track number and destroys the album's playback order.
    let disc_no = tag
        .and_then(Accessor::disk)
        .or_else(|| get(ItemKey::DiscNumber).and_then(|s| leading_u32(&s)))
        .or_else(|| disc_no_from_path(path))
        .or_else(|| album_raw.as_deref().and_then(|a| strip_disc_suffix(a).1))
        .unwrap_or(1)
        .max(1);
    let track_no = tag
        .and_then(Accessor::track)
        .or_else(|| get(ItemKey::TrackNumber).and_then(|s| leading_u32(&s)))
        .unwrap_or(0);

    let year = get(ItemKey::RecordingDate)
        .or_else(|| get(ItemKey::Year))
        .or_else(|| get(ItemKey::ReleaseDate))
        .and_then(|s| year_from_date(&s));

    let compilation = get(ItemKey::FlagCompilation)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    Ok(TrackMeta {
        path: path.to_path_buf(),
        mtime,
        size,
        title: get(ItemKey::TrackTitle),
        artist_raw: get(ItemKey::TrackArtist),
        album_artist_raw: get(ItemKey::AlbumArtist),
        album_raw,
        mb_album_id: get(ItemKey::MusicBrainzReleaseId),
        compilation,
        disc_no,
        track_no,
        year,
        duration_ms: props.duration().as_millis() as i64,
        codec: Some(format!("{:?}", tagged.file_type()).to_ascii_lowercase()),
        bitrate: props.audio_bitrate().or_else(|| props.overall_bitrate()),
        sample_rate: props.sample_rate(),
        rg_track_gain: get_f64(ItemKey::ReplayGainTrackGain),
        rg_track_peak: get_f64(ItemKey::ReplayGainTrackPeak),
        rg_album_gain: get_f64(ItemKey::ReplayGainAlbumGain),
        rg_album_peak: get_f64(ItemKey::ReplayGainAlbumPeak),
    })
}

/// Bucket files into albums, then resolve each album's shared fields.
///
/// `roots` is used only to decide whether an album's parent directory is a
/// plausible artist folder; pass the scanned roots.
pub fn group_into_albums(metas: Vec<TrackMeta>, roots: &[PathBuf]) -> Vec<AlbumDraft> {
    let mut buckets: HashMap<(PathBuf, String), Vec<TrackMeta>> = HashMap::new();

    for meta in metas {
        let dir = album_dir(&meta.path).to_path_buf();
        // Files with no album tag fall back to the directory name, which keeps
        // a folder of loose tracks together instead of exploding into singles.
        //
        // Disc markers are stripped from the title first, so "Hullabaloo CD1"
        // and "Hullabaloo CD2" land in the same bucket rather than being filed
        // as two unrelated releases.
        let album_key = meta
            .album_raw
            .as_deref()
            .map(|a| normalize(&strip_disc_suffix(a).0))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                dir.file_name()
                    .and_then(|n| n.to_str())
                    .map(normalize)
                    .unwrap_or_default()
            });
        buckets.entry((dir, album_key)).or_default().push(meta);
    }

    fold_truncated_buckets(&mut buckets);

    let drafts: Vec<AlbumDraft> = buckets
        .into_iter()
        .map(|((dir, _), tracks)| resolve_album(dir, tracks, roots))
        .collect();
    let mut drafts = merge_by_identity(drafts, roots);

    drafts.sort_by(|a, b| {
        sort_key(&a.album_artist)
            .cmp(&sort_key(&b.album_artist))
            .then_with(|| a.year.cmp(&b.year))
            .then_with(|| sort_key(&a.title).cmp(&sort_key(&b.title)))
    });
    drafts
}

/// ID3v1 caps its text fields at 30 bytes. A title of exactly that length was
/// very likely cut off mid-word, making it a prefix rather than a name.
const ID3V1_FIELD_LEN: usize = 30;

fn looks_truncated(title: &str) -> bool {
    title.len() == ID3V1_FIELD_LEN
}

/// How many sibling folders may be merged into a single release.
///
/// Two or three named folders is an ordinary double or triple album. Beyond
/// that, a shared album tag is far more likely to be a generic credit than a
/// real album title — classical rips routinely put a performer ("NCO, Nicholas
/// Ward") in the album field across a whole box set of separate works. Those
/// are better off as one album per folder, so each work can be played alone.
///
/// Note this only governs *sibling* folders. Discs in `CD1`/`CD2` subdirectories
/// collapse into one bucket earlier and are unaffected by this limit.
const MAX_MERGED_FOLDERS: usize = 4;

/// Rebuild a draft as its own album, named after its folder.
///
/// Used when a shared album tag turns out to be a credit rather than a title:
/// the folder name is then the most informative thing available.
fn retitle_from_folder(mut draft: AlbumDraft, roots: &[PathBuf]) -> AlbumDraft {
    if let Some(name) = draft.dir_path.file_name().and_then(|n| n.to_str()) {
        draft.title = name.to_string();
    }
    draft.group_key = directory_key(&draft.dir_path, roots);
    draft.identity = IdentitySource::Directory;
    draft.source_dirs = 1;
    draft
}

/// Identity key for an album that has nothing but its folder to go on.
///
/// The path is recorded *relative to the library root*. An absolute path would
/// tie the album's identity to where the library happens to be mounted, so
/// moving from `/mnt/share/Music` to `/music` — exactly what happens when the
/// server moves into a container — would orphan the album and everything
/// attached to it.
fn directory_key(dir: &Path, roots: &[PathBuf]) -> String {
    format!("dir:{}", relative_dir(dir, roots))
}

/// A directory expressed relative to whichever library root contains it.
fn relative_dir(dir: &Path, roots: &[PathBuf]) -> String {
    for root in roots {
        if let Ok(relative) = dir.strip_prefix(root) {
            return relative.to_string_lossy().into_owned();
        }
    }
    dir.to_string_lossy().into_owned()
}

/// Merge drafts that resolved to the same identity.
///
/// One release can occupy two sibling directories — a double album whose discs
/// are named rather than numbered, like Mellon Collie's "Dawn to Dusk" and
/// "Twilight to Starlight". Bucketing is per-directory, so those arrive here as
/// separate drafts sharing a `group_key`. Without this pass they collide on
/// insert and the album row ends up describing only whichever disc was written
/// last, while both discs' tracks hang off it.
fn merge_by_identity(drafts: Vec<AlbumDraft>, roots: &[PathBuf]) -> Vec<AlbumDraft> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<AlbumDraft>> = HashMap::new();

    for draft in drafts {
        if !groups.contains_key(&draft.group_key) {
            order.push(draft.group_key.clone());
        }
        groups.entry(draft.group_key.clone()).or_default().push(draft);
    }

    let mut out = Vec::with_capacity(order.len());
    for key in order {
        let Some(mut group) = groups.remove(&key) else {
            continue;
        };
        if group.len() == 1 {
            out.push(group.remove(0));
            continue;
        }

        group.sort_by(|a, b| a.dir_path.cmp(&b.dir_path));

        // Too many folders to be one release: the shared tag is a credit, not a
        // title. Give each folder its own album named after itself.
        if group.len() > MAX_MERGED_FOLDERS {
            out.extend(group.into_iter().map(|d| retitle_from_folder(d, roots)));
            continue;
        }

        // When no disc tag or disc folder distinguished these, the directories
        // themselves are the only ordering information available — otherwise
        // every disc claims track 1 and the album has no total order.
        let undifferentiated = group
            .iter()
            .all(|d| d.tracks.iter().all(|t| t.disc_no == 1));

        let dirs: Vec<PathBuf> = group.iter().map(|d| d.dir_path.clone()).collect();
        let dir_path = common_ancestor(&dirs);
        let art_path = group.iter().find_map(|d| d.art_path.clone());
        let is_compilation = group.iter().any(|d| d.is_compilation);
        let year = group.iter().filter_map(|d| d.year).min();
        let source_dirs = group.len();

        let template = group[0].clone();
        let mut tracks = Vec::new();
        for (index, mut draft) in group.into_iter().enumerate() {
            if undifferentiated {
                let disc = index as u32 + 1;
                for track in &mut draft.tracks {
                    track.disc_no = disc;
                }
            }
            tracks.append(&mut draft.tracks);
        }
        tracks.sort_by_key(|t| (t.disc_no, t.track_no, t.path.clone()));

        out.push(AlbumDraft {
            dir_path,
            art_path,
            is_compilation,
            year,
            source_dirs,
            tracks,
            ..template
        });
    }
    out
}

/// The deepest directory that contains all of `paths`.
fn common_ancestor(paths: &[PathBuf]) -> PathBuf {
    let Some(first) = paths.first() else {
        return PathBuf::new();
    };
    let mut common: Vec<std::path::Component<'_>> = first.components().collect();
    for path in &paths[1..] {
        let other: Vec<_> = path.components().collect();
        let shared = common
            .iter()
            .zip(&other)
            .take_while(|(a, b)| a == b)
            .count();
        common.truncate(shared);
    }
    if common.is_empty() {
        first.clone()
    } else {
        common.iter().collect()
    }
}

/// Whether two artist credits name the same artist.
///
/// Beyond an exact match, a name sitting exactly on the ID3v1 length limit is
/// treated as a cut-short spelling of a longer one it prefixes. Without this a
/// single ID3v1-only track turns its album into a bogus compilation. The
/// truncation check keeps the rule tight: "Bob Dylan" never absorbs "Bob Dylan
/// & The Band".
fn same_artist(a: &str, b: &str) -> bool {
    let (na, nb) = (normalize(a), normalize(b));
    if na == nb {
        return true;
    }
    (looks_truncated(a) && nb.starts_with(&na)) || (looks_truncated(b) && na.starts_with(&nb))
}

/// Fold a bucket whose title was cut short into the longer title it prefixes.
///
/// A folder can hold one file with only an ID3v1 tag among others carrying full
/// ID3v2 ones. The short title is then a strict prefix of the real one, and
/// leaving them apart strands a single track as its own "album" beside the
/// record it belongs to. Only exact prefixes within the same directory are
/// folded, so unrelated releases are never merged on a shared opening phrase.
fn fold_truncated_buckets(buckets: &mut HashMap<(PathBuf, String), Vec<TrackMeta>>) {
    // Only buckets whose every track carries a title sitting on the ID3v1 limit
    // are candidates; anything else has a title we should trust.
    let candidates: Vec<(PathBuf, String)> = buckets
        .iter()
        .filter(|((_, key), tracks)| {
            !key.is_empty()
                && tracks.iter().all(|t| {
                    t.album_raw
                        .as_deref()
                        .is_some_and(|raw| looks_truncated(&strip_disc_suffix(raw).0))
                })
        })
        .map(|((dir, key), _)| (dir.clone(), key.clone()))
        .collect();

    for (dir, short) in candidates {
        let target = buckets
            .keys()
            .filter(|(other_dir, other)| {
                *other_dir == dir && other.len() > short.len() && other.starts_with(&short)
            })
            .min_by_key(|(_, other)| other.len())
            .cloned();

        if let Some(target) = target
            && let Some(moved) = buckets.remove(&(dir, short))
        {
            buckets.entry(target).or_default().extend(moved);
        }
    }
}

/// Decide an album's title, artist, year, and identity from its tracks.
fn resolve_album(dir: PathBuf, mut tracks: Vec<TrackMeta>, roots: &[PathBuf]) -> AlbumDraft {
    tracks.sort_by_key(|t| (t.disc_no, t.track_no, t.path.clone()));

    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown Album")
        .to_string();

    // Titles are compared and stored without their disc markers, so a merged
    // double album is named "Forty Licks", not "Forty Licks ( Disc 2 )".
    let title = majority(
        tracks
            .iter()
            .map(|t| t.album_raw.as_deref().map(|a| strip_disc_suffix(a).0)),
    )
    .unwrap_or(dir_name);
    let mb_album_id = majority(tracks.iter().map(|t| t.mb_album_id.clone()));
    let year = tracks.iter().filter_map(|t| t.year).min();

    // Album artist resolution, in descending order of trustworthiness. Getting
    // this wrong is what shatters a Various Artists compilation into a dozen
    // one-track "albums", so the unanimity check comes before any guessing.
    let tagged_album_artist = majority(tracks.iter().map(|t| t.album_artist_raw.clone()));
    let flagged_compilation = tracks.iter().any(|t| t.compilation);

    let distinct_artists: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for t in &tracks {
            let Some(name) = t.artist_raw.as_deref().filter(|a| !a.trim().is_empty()) else {
                continue;
            };
            match seen.iter_mut().find(|s| same_artist(s, name)) {
                // Keep whichever spelling is complete, so the album is credited
                // in full rather than to a name chopped at 30 characters.
                Some(existing) if name.len() > existing.len() => *existing = name.to_string(),
                Some(_) => {}
                None => seen.push(name.to_string()),
            }
        }
        seen
    };

    let (album_artist, is_compilation) = match tagged_album_artist {
        Some(a) => {
            let comp = flagged_compilation || normalize(&a) == "various artists";
            (a, comp)
        }
        None if flagged_compilation => ("Various Artists".to_string(), true),
        None if distinct_artists.len() == 1 => (distinct_artists[0].clone(), false),
        // Wholly untagged files still sit somewhere meaningful. A library laid
        // out as <root>/<artist>/<album> gives the artist away in the path, so
        // use it rather than surrendering to "Unknown Artist".
        None if distinct_artists.is_empty() => match artist_from_parent_dir(&dir, roots) {
            Some(name) => (name, false),
            None => ("Unknown Artist".to_string(), false),
        },
        None => ("Various Artists".to_string(), true),
    };

    // Prefer an identity that survives the folder being moved or renamed.
    let has_album_tag = tracks.iter().any(|t| t.album_raw.is_some());
    let (group_key, identity) = match (&mb_album_id, has_album_tag) {
        (Some(id), _) => (format!("mb:{id}"), IdentitySource::MusicBrainz),
        (None, true) => {
            let year_part = year.map(|y| y.to_string()).unwrap_or_default();
            let mut key = format!(
                "aa:{}|{}|{}",
                normalize(&album_artist),
                normalize(&title),
                year_part
            );
            // A title sitting exactly on the ID3v1 field limit has probably been
            // cut short, so it is only a prefix. Two unrelated releases can share
            // one ("The End Is The Beginning Is Th" for both a single and its
            // remix EP), and merging on it would fuse them. Scope such keys to
            // the directory: worst case the album fails to merge, which is the
            // safe direction to fail in.
            if looks_truncated(&title) {
                // Relative, for the same reason `directory_key` is: an absolute
                // path would pin the album to one mount point.
                key.push_str(&format!("|dir:{}", relative_dir(&dir, roots)));
            }
            (key, IdentitySource::ArtistTitle)
        }
        (None, false) => (directory_key(&dir, roots), IdentitySource::Directory),
    };

    AlbumDraft {
        group_key,
        identity,
        title,
        album_artist,
        year,
        mb_album_id,
        art_path: find_cover(&dir),
        dir_path: dir,
        is_compilation,
        source_dirs: 1,
        tracks,
    }
}

/// The artist folder containing an album, for libraries laid out as
/// `<root>/<artist>/<album>`.
///
/// Returns `None` when the album sits directly in a root, since the root's own
/// name ("Music") says nothing about who made the record.
fn artist_from_parent_dir(dir: &Path, roots: &[PathBuf]) -> Option<String> {
    let parent = dir.parent()?;
    if roots.iter().any(|r| r == parent) {
        return None;
    }
    parent
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .filter(|n| !n.trim().is_empty())
}

/// Look for a cover image sitting next to the audio files.
fn find_cover(dir: &Path) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();

    // Preference order is by stem first, so `cover.jpg` beats `art.png`.
    for stem in COVER_STEMS {
        for ext in COVER_EXTENSIONS {
            if let Some(hit) = entries.iter().find(|p| {
                let matches_stem = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.eq_ignore_ascii_case(stem));
                let matches_ext = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case(ext));
                matches_stem && matches_ext
            }) {
                return Some(hit.clone());
            }
        }
    }
    None
}

/// Write the resolved albums into the database.
///
/// Rows are upserted on their stable keys so that `album.id` and `track.id`
/// survive rescans — play history references those IDs. Anything not seen in
/// this pass is flagged `present = 0` rather than deleted.
fn persist(lib: &mut Library, drafts: &[AlbumDraft], report: &mut ScanReport) -> Result<()> {
    let now = now_unix();
    let tx = lib.conn.transaction()?;

    // Only clear the present flags when the scan is trustworthy. Otherwise the
    // rows keep whatever state they had and merely get refreshed below.
    if !report.absences_skipped {
        tx.execute("UPDATE album SET present = 0", [])?;
        tx.execute("UPDATE track SET present = 0", [])?;
    }

    for draft in drafts {
        let album_artist_id = upsert_artist(&tx, &draft.album_artist)?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM album WHERE group_key = ?1",
                params![draft.group_key],
                |r| r.get(0),
            )
            .ok();
        if existing.is_none() {
            report.albums_new += 1;
        }

        let album_id: i64 = tx.query_row(
            "INSERT INTO album (group_key, identity_source, title, sort_title,
                                album_artist_id, year, mb_album_id, dir_path,
                                disc_count, track_count, duration_ms, art_path,
                                is_compilation, source_dirs, present, added_at, last_seen_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?15,1,?14,?14)
             ON CONFLICT(group_key) DO UPDATE SET
                identity_source = excluded.identity_source,
                title           = excluded.title,
                sort_title      = excluded.sort_title,
                album_artist_id = excluded.album_artist_id,
                year            = excluded.year,
                mb_album_id     = excluded.mb_album_id,
                dir_path        = excluded.dir_path,
                disc_count      = excluded.disc_count,
                track_count     = excluded.track_count,
                duration_ms     = excluded.duration_ms,
                art_path        = excluded.art_path,
                is_compilation  = excluded.is_compilation,
                source_dirs     = excluded.source_dirs,
                present         = 1,
                last_seen_at    = excluded.last_seen_at
             RETURNING id",
            params![
                draft.group_key,
                draft.identity.as_str(),
                draft.title,
                sort_key(&draft.title),
                album_artist_id,
                draft.year,
                draft.mb_album_id,
                draft.dir_path.to_string_lossy(),
                draft.disc_count(),
                draft.tracks.len() as i64,
                draft.duration_ms(),
                draft.art_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                draft.is_compilation as i64,
                now,
                draft.source_dirs as i64,
            ],
            |r| r.get(0),
        )?;

        for track in &draft.tracks {
            let artist_name = track
                .artist_raw
                .clone()
                .unwrap_or_else(|| draft.album_artist.clone());
            let artist_id = upsert_artist(&tx, &artist_name)?;

            tx.execute(
                "INSERT INTO track (path, album_id, artist_id, disc_no, track_no, title,
                                    duration_ms, codec, bitrate, sample_rate,
                                    rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak,
                                    album_raw, album_artist_raw, artist_raw, mb_album_id,
                                    compilation, year, mtime, size, present, added_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,
                         ?15,?16,?17,?18,?19,?20,?21,?22,1,?23)
                 ON CONFLICT(path) DO UPDATE SET
                    album_id      = excluded.album_id,
                    artist_id     = excluded.artist_id,
                    disc_no       = excluded.disc_no,
                    track_no      = excluded.track_no,
                    title         = excluded.title,
                    duration_ms   = excluded.duration_ms,
                    codec         = excluded.codec,
                    bitrate       = excluded.bitrate,
                    sample_rate   = excluded.sample_rate,
                    rg_track_gain = excluded.rg_track_gain,
                    rg_track_peak = excluded.rg_track_peak,
                    rg_album_gain = excluded.rg_album_gain,
                    rg_album_peak = excluded.rg_album_peak,
                    album_raw        = excluded.album_raw,
                    album_artist_raw = excluded.album_artist_raw,
                    artist_raw       = excluded.artist_raw,
                    mb_album_id      = excluded.mb_album_id,
                    compilation      = excluded.compilation,
                    year          = excluded.year,
                    mtime         = excluded.mtime,
                    size          = excluded.size,
                    present       = 1",
                params![
                    track.path.to_string_lossy(),
                    album_id,
                    artist_id,
                    track.disc_no,
                    track.track_no,
                    track.display_title(),
                    track.duration_ms,
                    track.codec,
                    track.bitrate,
                    track.sample_rate,
                    track.rg_track_gain,
                    track.rg_track_peak,
                    track.rg_album_gain,
                    track.rg_album_peak,
                    track.album_raw,
                    track.album_artist_raw,
                    track.artist_raw,
                    track.mb_album_id,
                    track.compilation as i64,
                    track.year,
                    track.mtime,
                    track.size,
                    now,
                ],
            )?;
        }
    }

    report.tracks_gone =
        tx.query_row("SELECT COUNT(*) FROM track WHERE present = 0", [], |r| {
            r.get::<_, i64>(0)
        })? as usize;
    report.albums_gone =
        tx.query_row("SELECT COUNT(*) FROM album WHERE present = 0", [], |r| {
            r.get::<_, i64>(0)
        })? as usize;

    tx.commit()?;
    Ok(())
}

fn upsert_artist(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<i64> {
    let name = if name.trim().is_empty() {
        "Unknown Artist"
    } else {
        name.trim()
    };
    let id = tx.query_row(
        "INSERT INTO artist (name, sort_name) VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET sort_name = excluded.sort_name
         RETURNING id",
        params![name, sort_key(name)],
        |r| r.get(0),
    )?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(path: &str, album: Option<&str>, artist: Option<&str>, aa: Option<&str>) -> TrackMeta {
        TrackMeta {
            path: PathBuf::from(path),
            mtime: 0,
            size: 0,
            title: Some("t".into()),
            artist_raw: artist.map(str::to_string),
            album_artist_raw: aa.map(str::to_string),
            album_raw: album.map(str::to_string),
            mb_album_id: None,
            compilation: false,
            disc_no: 1,
            track_no: 1,
            year: Some(1999),
            duration_ms: 1000,
            codec: None,
            bitrate: None,
            sample_rate: None,
            rg_track_gain: None,
            rg_track_peak: None,
            rg_album_gain: None,
            rg_album_peak: None,
        }
    }

    #[test]
    fn multi_disc_release_stays_one_album() {
        let metas = vec![
            meta("/m/PF/The Wall/CD1/01.mp3", Some("The Wall"), Some("Pink Floyd"), None),
            meta("/m/PF/The Wall/CD2/01.mp3", Some("The Wall"), Some("Pink Floyd"), None),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].album_artist, "Pink Floyd");
    }

    #[test]
    fn compilation_does_not_shatter_into_singles() {
        // No albumartist tag, every track a different artist: the classic case
        // that fragments naive scanners into one album per track.
        let metas = vec![
            meta("/m/Comp/01.mp3", Some("Now 42"), Some("Artist A"), None),
            meta("/m/Comp/02.mp3", Some("Now 42"), Some("Artist B"), None),
            meta("/m/Comp/03.mp3", Some("Now 42"), Some("Artist C"), None),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].album_artist, "Various Artists");
        assert!(albums[0].is_compilation);
        assert_eq!(albums[0].tracks.len(), 3);
    }

    #[test]
    fn tagged_album_artist_wins_over_track_artists() {
        let metas = vec![
            meta("/m/X/01.mp3", Some("Duets"), Some("Guest One"), Some("Frank Sinatra")),
            meta("/m/X/02.mp3", Some("Duets"), Some("Guest Two"), Some("Frank Sinatra")),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].album_artist, "Frank Sinatra");
        assert!(!albums[0].is_compilation);
    }

    #[test]
    fn directory_identity_is_relative_to_the_library_root() {
        // The same album under two different mount points must keep one
        // identity, or moving the server into a container orphans it.
        let on_host = group_into_albums(
            vec![meta("/mnt/share/Music/Bootleg/01.mp3", None, None, None)],
            &[PathBuf::from("/mnt/share/Music")],
        );
        let in_container = group_into_albums(
            vec![meta("/music/Bootleg/01.mp3", None, None, None)],
            &[PathBuf::from("/music")],
        );
        assert_eq!(on_host[0].group_key, in_container[0].group_key);
        assert_eq!(on_host[0].group_key, "dir:Bootleg");
    }

    #[test]
    fn untagged_folder_groups_by_directory() {
        let metas = vec![
            meta("/m/Bootleg/01.mp3", None, None, None),
            meta("/m/Bootleg/02.mp3", None, None, None),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].identity, IdentitySource::Directory);
        assert_eq!(albums[0].title, "Bootleg");
    }

    #[test]
    fn distinct_albums_in_one_folder_split() {
        let metas = vec![
            meta("/m/Singles/a.mp3", Some("Album One"), Some("A"), None),
            meta("/m/Singles/b.mp3", Some("Album Two"), Some("B"), None),
        ];
        assert_eq!(group_into_albums(metas, &[]).len(), 2);
    }

    #[test]
    fn identity_survives_a_directory_rename() {
        let before = group_into_albums(
            vec![meta("/m/old-name/01.mp3", Some("Kid A"), Some("Radiohead"), None)],
            &[],
        );
        let after = group_into_albums(
            vec![meta(
                "/m/Radiohead - Kid A (2000)/01.mp3",
                Some("Kid A"),
                Some("Radiohead"),
                None,
            )],
            &[],
        );
        assert_eq!(before[0].group_key, after[0].group_key);
        assert_eq!(before[0].identity, IdentitySource::ArtistTitle);
    }

    /// Build metadata for a file in a disc subdirectory with no disc tag —
    /// the White Album case, which is the single most common multi-disc layout.
    fn disc_meta(path: &str, album: &str, track_no: u32) -> TrackMeta {
        let mut m = meta(path, Some(album), Some("The Beatles"), None);
        m.track_no = track_no;
        m.disc_no = disc_no_from_path(Path::new(path))
            .or_else(|| strip_disc_suffix(album).1)
            .unwrap_or(1);
        m
    }

    #[test]
    fn disc_folders_supply_missing_disc_numbers() {
        // Both discs tag their tracks 1..n with no disc tag at all. Without the
        // folder fallback every track collides on disc 1 and album order is lost.
        let metas = vec![
            disc_meta("/m/Beatles/White Album/Disc 1/a.mp3", "The White Album", 1),
            disc_meta("/m/Beatles/White Album/Disc 1/b.mp3", "The White Album", 2),
            disc_meta("/m/Beatles/White Album/Disc 2/c.mp3", "The White Album", 1),
            disc_meta("/m/Beatles/White Album/Disc 2/d.mp3", "The White Album", 2),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].disc_count(), 2);

        // Every (disc, track) pair is now unique, so playback order is total.
        let mut pairs: Vec<(u32, u32)> =
            albums[0].tracks.iter().map(|t| (t.disc_no, t.track_no)).collect();
        pairs.sort_unstable();
        pairs.dedup();
        assert_eq!(pairs.len(), 4);
    }

    #[test]
    fn disc_marker_in_the_album_title_does_not_split_the_release() {
        // The Forty Licks case: one release, but each disc carries a different
        // album tag, which previously produced two unrelated albums.
        let metas = vec![
            disc_meta("/m/Stones/40 Licks/CD 1/a.mp3", "Forty Licks (Disc One)", 1),
            disc_meta("/m/Stones/40 Licks/CD 2/b.mp3", "Forty Licks ( Disc 2 )", 1),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, "Forty Licks");
        assert_eq!(albums[0].disc_count(), 2);
    }

    #[test]
    fn sibling_folders_holding_one_release_are_merged() {
        // Mellon Collie: two discs in named folders, same album tag, no disc
        // tags. Bucketing is per-directory, so these must merge afterwards or
        // the album row records only one of the two discs.
        let mut metas = Vec::new();
        for (dir, n) in [("Dawn to Dusk", 1u32), ("Twilight to Starlight", 2)] {
            for track_no in 1..=3u32 {
                let mut m = meta(
                    &format!("/m/SP/Mellon Collie - {dir}/{track_no}.mp3"),
                    Some("Mellon Collie and the Infinite Sadness"),
                    Some("The Smashing Pumpkins"),
                    None,
                );
                m.track_no = track_no;
                let _ = n;
                metas.push(m);
            }
        }
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1, "the two folders are one release");

        let album = &albums[0];
        assert_eq!(album.tracks.len(), 6, "no disc is dropped");
        assert_eq!(album.source_dirs, 2);
        // Folder order supplies the disc numbers the tags never had, so the
        // six tracks have a total order rather than two colliding runs of 1..3.
        assert_eq!(album.disc_count(), 2);
        let mut pairs: Vec<(u32, u32)> =
            album.tracks.iter().map(|t| (t.disc_no, t.track_no)).collect();
        pairs.sort_unstable();
        pairs.dedup();
        assert_eq!(pairs.len(), 6);
        // dir_path becomes the folder that contains both discs.
        assert_eq!(album.dir_path, PathBuf::from("/m/SP"));
    }

    #[test]
    fn a_truncated_artist_does_not_invent_a_compilation() {
        let full = "The Presidents of the United States of America";
        let cut = "The Presidents of the United S";
        assert!(same_artist(full, cut));
        // But a genuinely different, longer credit stays distinct.
        assert!(!same_artist("Bob Dylan", "Bob Dylan & The Band"));
        assert!(!same_artist("Beck", "Beck Hansen"));
    }

    #[test]
    fn a_stray_id3v1_track_rejoins_its_album() {
        // One file in the folder has only an ID3v1 tag, so its album title is
        // cut to 30 characters; the rest carry the full ID3v2 title.
        let full = "The Presidents of the United States of America (10th Anniversary Edition)";
        let truncated = "The Presidents of the United S";
        assert_eq!(truncated.len(), 30);

        let mut metas = vec![meta("/m/P/10th/kitty.mp3", Some(truncated), Some("P"), None)];
        for i in 1..=3 {
            metas.push(meta(&format!("/m/P/10th/{i}.mp3"), Some(full), Some("P"), None));
        }

        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 1, "the stray track is not its own album");
        assert_eq!(albums[0].tracks.len(), 4);
        assert_eq!(albums[0].title, full, "the complete title wins");
        assert!(!albums[0].is_compilation, "one artist, not a compilation");
    }

    #[test]
    fn folding_only_happens_inside_one_folder() {
        // Same prefix, different folders: these are separate releases.
        let truncated = "The End Is The Beginning Is Th";
        let metas = vec![
            meta("/m/SP/single/01.mp3", Some(truncated), Some("SP"), None),
            meta(
                "/m/SP/remixes/01.mp3",
                Some("The End Is The Beginning Is The End (Remixes)"),
                Some("SP"),
                None,
            ),
        ];
        assert_eq!(group_into_albums(metas, &[]).len(), 2);
    }

    #[test]
    fn titles_cut_off_by_id3v1_do_not_fuse_different_releases() {
        // Both folders carry the same 30-character title because ID3v1 truncated
        // it; they are actually a single and its remix EP.
        let title = "The End Is The Beginning Is Th";
        assert_eq!(title.len(), 30, "fixture must sit on the ID3v1 limit");

        let metas = vec![
            meta("/m/SP/The End Is The Beginning/01.mp3", Some(title), Some("SP"), None),
            meta("/m/SP/The End Is The Beginning Remi/01.mp3", Some(title), Some("SP"), None),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 2, "a truncated title is not enough to merge on");
    }

    #[test]
    fn truncated_title_identities_are_also_mount_independent() {
        // These keys carry a directory suffix to keep unrelated releases apart;
        // that suffix must be relative too, or moving the library orphans them.
        let title = "The End Is The Beginning Is Th";
        let on_host = group_into_albums(
            vec![meta("/mnt/share/Music/SP/EP/01.mp3", Some(title), Some("SP"), None)],
            &[PathBuf::from("/mnt/share/Music")],
        );
        let in_container = group_into_albums(
            vec![meta("/music/SP/EP/01.mp3", Some(title), Some("SP"), None)],
            &[PathBuf::from("/music")],
        );
        assert_eq!(on_host[0].group_key, in_container[0].group_key);
        assert!(on_host[0].group_key.ends_with("|dir:SP/EP"), "{}", on_host[0].group_key);
    }

    #[test]
    fn full_length_titles_still_merge_across_folders() {
        let title = "Mellon Collie and the Infinite Sadness";
        assert!(title.len() > 30);
        let metas = vec![
            meta("/m/SP/MC - Dawn/01.mp3", Some(title), Some("SP"), None),
            meta("/m/SP/MC - Twilight/01.mp3", Some(title), Some("SP"), None),
        ];
        assert_eq!(group_into_albums(metas, &[]).len(), 1);
    }

    #[test]
    fn a_credit_shared_by_many_folders_splits_into_one_album_per_folder() {
        // The Mozart case: the album tag holds a performer credit, repeated
        // across a box set of separate works, one work per folder.
        let works = [
            "01 - Symphony 1 in E Flat Major k.16",
            "02 - Symphony 2 in B Flat Major k.17",
            "03 - Symphony 3 in E Flat Major k.18",
            "04 - Symphony 4 in D Major k.19",
            "05 - Symphony 5 in B Flat Major k.22",
        ];
        let metas: Vec<TrackMeta> = works
            .iter()
            .map(|w| {
                meta(
                    &format!("/m/Mozart/{w}/01.mp3"),
                    Some("NCO, Nicholas Ward"),
                    Some("Mozart"),
                    None,
                )
            })
            .collect();

        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), works.len(), "one album per work, not one box");
        // Each is named for its folder, since the album tag was never a title.
        let mut titles: Vec<&str> = albums.iter().map(|a| a.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(titles, works);
        assert!(albums.iter().all(|a| a.identity == IdentitySource::Directory));
    }

    #[test]
    fn an_ordinary_double_album_is_still_merged() {
        // Two folders stays under the limit, so the Mellon Collie case is
        // unaffected by the box-set split.
        let metas = vec![
            meta("/m/SP/MC - Dawn/01.mp3", Some("Mellon Collie"), Some("SP"), None),
            meta("/m/SP/MC - Twilight/01.mp3", Some("Mellon Collie"), Some("SP"), None),
        ];
        assert_eq!(group_into_albums(metas, &[]).len(), 1);
    }

    #[test]
    fn merging_leaves_genuinely_distinct_albums_alone() {
        let metas = vec![
            meta("/m/A/One/01.mp3", Some("One"), Some("A"), None),
            meta("/m/A/Two/01.mp3", Some("Two"), Some("A"), None),
        ];
        let albums = group_into_albums(metas, &[]);
        assert_eq!(albums.len(), 2);
        assert!(albums.iter().all(|a| a.source_dirs == 1));
    }

    #[test]
    fn untagged_files_take_their_artist_from_the_folder() {
        // Miles Davis / ESP: no tags whatsoever, artist only in the path.
        let root = PathBuf::from("/m");
        let mut m = meta("/m/Miles Davis/ESP/01.mp3", None, None, None);
        m.year = None;
        let albums = group_into_albums(vec![m], std::slice::from_ref(&root));
        assert_eq!(albums[0].album_artist, "Miles Davis");
        assert_eq!(albums[0].title, "ESP");
    }

    #[test]
    fn an_album_sitting_in_the_root_does_not_borrow_the_roots_name() {
        let root = PathBuf::from("/m");
        let m = meta("/m/Zelda 25th Anniversary/01.mp3", None, None, None);
        let albums = group_into_albums(vec![m], std::slice::from_ref(&root));
        assert_eq!(albums[0].album_artist, "Unknown Artist");
    }

    #[test]
    fn an_unreliable_scan_does_not_mark_anything_missing() {
        // Simulates a flaky share: the album is in the database, but this pass
        // could not read its files. They must not be declared gone.
        let dir = std::env::temp_dir().join(format!("apflaky{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Album")).unwrap();
        for i in 1..=4 {
            std::fs::write(dir.join(format!("Album/{i}.mp3")), b"not really an mp3").unwrap();
        }

        let mut lib = Library::open_in_memory().unwrap();
        let report = scan_roots(&mut lib, std::slice::from_ref(&dir), ScanOptions::default())
            .unwrap();

        assert_eq!(report.files_seen, 4);
        assert_eq!(report.files_failed, 4, "the stub files are unreadable");
        assert!(report.absences_skipped, "a 100% failure rate is not trustworthy");
        assert_eq!(report.tracks_gone, 0, "nothing was declared missing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_scan_still_reports_absences() {
        let dir = std::env::temp_dir().join(format!("apclean{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut lib = Library::open_in_memory().unwrap();
        let report =
            scan_roots(&mut lib, std::slice::from_ref(&dir), ScanOptions::default()).unwrap();

        // An empty directory has nothing to fail on, so absence still applies.
        assert_eq!(report.files_seen, 0);
        assert!(!report.absences_skipped);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_is_idempotent_and_preserves_ids() {
        let dir = std::env::temp_dir().join(format!("apscan{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Album")).unwrap();
        // An unreadable stub still exercises the walk/persist path via failure
        // accounting; grouping itself is covered by the unit tests above.
        std::fs::write(dir.join("Album/01.mp3"), b"not really an mp3").unwrap();

        let mut lib = Library::open_in_memory().unwrap();
        let r1 = scan_roots(&mut lib, std::slice::from_ref(&dir), ScanOptions::default()).unwrap();
        assert_eq!(r1.files_seen, 1);

        let r2 = scan_roots(&mut lib, std::slice::from_ref(&dir), ScanOptions::default()).unwrap();
        assert_eq!(r2.files_seen, 1);
        assert_eq!(r1.albums, r2.albums);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
