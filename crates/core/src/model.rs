//! Domain types. The album is the primary unit here, not the track.

use std::path::PathBuf;

/// Everything the scanner reads out of one audio file, before any grouping
/// decisions are made. The `*_raw` fields are kept verbatim so that a rescan
/// can regroup from the database without re-reading tags off disk.
#[derive(Debug, Clone)]
pub struct TrackMeta {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: i64,

    pub title: Option<String>,
    pub artist_raw: Option<String>,
    pub album_artist_raw: Option<String>,
    pub album_raw: Option<String>,
    pub mb_album_id: Option<String>,
    pub compilation: bool,

    pub disc_no: u32,
    pub track_no: u32,
    pub year: Option<i32>,

    pub duration_ms: i64,
    pub codec: Option<String>,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,

    pub rg_track_gain: Option<f64>,
    pub rg_track_peak: Option<f64>,
    pub rg_album_gain: Option<f64>,
    pub rg_album_peak: Option<f64>,
}

impl TrackMeta {
    /// Display title, falling back to the file stem for untagged files.
    pub fn display_title(&self) -> String {
        self.title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                self.path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            })
    }
}

/// How an album's identity was established, surfaced by `doctor` so you can see
/// which albums rest on weak evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// A MusicBrainz release ID was present — the strongest signal.
    MusicBrainz,
    /// Resolved album artist plus album title (plus year when tagged).
    ArtistTitle,
    /// No usable album tag; the directory path is all we have.
    Directory,
}

impl IdentitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MusicBrainz => "musicbrainz",
            Self::ArtistTitle => "artist+title",
            Self::Directory => "directory",
        }
    }
}

/// An album assembled from a group of files, with per-album fields resolved by
/// majority vote across its tracks.
#[derive(Debug, Clone)]
pub struct AlbumDraft {
    /// Stable identity used as the database key. Survives directory renames
    /// whenever tags are good enough to identify the release.
    pub group_key: String,
    pub identity: IdentitySource,
    pub title: String,
    pub album_artist: String,
    pub year: Option<i32>,
    pub mb_album_id: Option<String>,
    pub dir_path: PathBuf,
    pub art_path: Option<PathBuf>,
    pub is_compilation: bool,
    /// How many directories this album was assembled from. More than one means
    /// separate folders resolved to the same release and were merged.
    pub source_dirs: usize,
    pub tracks: Vec<TrackMeta>,
}

impl AlbumDraft {
    pub fn disc_count(&self) -> u32 {
        self.tracks.iter().map(|t| t.disc_no).max().unwrap_or(1).max(1)
    }

    pub fn duration_ms(&self) -> i64 {
        self.tracks.iter().map(|t| t.duration_ms).sum()
    }
}

/// Row-level view of an album for listings.
#[derive(Debug, Clone)]
pub struct AlbumRow {
    pub id: i64,
    pub title: String,
    pub album_artist: String,
    pub year: Option<i32>,
    pub track_count: i64,
    pub disc_count: i64,
    pub duration_ms: i64,
    pub is_compilation: bool,
    pub dir_path: String,
    pub source_dirs: i64,
    pub play_count: i64,
    pub last_played: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ArtistRow {
    pub id: i64,
    pub name: String,
    pub album_count: i64,
    pub track_plays: i64,
    pub album_plays: i64,
    pub last_played: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TrackRow {
    pub id: i64,
    pub disc_no: i64,
    pub track_no: i64,
    pub title: String,
    pub artist: String,
    pub duration_ms: i64,
    pub codec: Option<String>,
    pub path: String,
    pub play_count: i64,
}

/// Aggregate counts for the `stats` command.
#[derive(Debug, Clone, Default)]
pub struct LibraryStats {
    pub albums: i64,
    pub artists: i64,
    pub tracks: i64,
    pub total_duration_ms: i64,
    pub compilations: i64,
    pub missing_albums: i64,
    pub missing_tracks: i64,
    pub album_plays: i64,
    pub track_plays: i64,
}
