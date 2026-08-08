//! Small helpers shared by the scanner and the query layer.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch. Timestamps are stored as INTEGER throughout.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Lowercase, trim, and collapse internal whitespace. Used to build grouping
/// keys so that "The  Beatles " and "the beatles" land in the same bucket.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.extend(ch.to_lowercase());
    }
    out
}

/// Sort key that moves leading articles to the end, so "The Beatles" files
/// under B. Returns a normalized string intended for ORDER BY, not display.
pub fn sort_key(s: &str) -> String {
    let n = normalize(s);
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = n.strip_prefix(article) {
            return rest.to_string();
        }
    }
    n
}

/// Disc keywords recognized in directory names and album titles.
const DISC_WORDS: &[&str] = &["disc", "disk", "cd"];

/// Spelled-out disc numbers, which appear in real tags as often as digits
/// ("Forty Licks (Disc One)").
const NUMBER_WORDS: &[(&str, u32)] = &[
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
    ("eleven", 11),
    ("twelve", 12),
];

/// Parse a disc number written as digits or as a word.
fn parse_disc_number(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse().ok();
    }
    NUMBER_WORDS
        .iter()
        .find(|(w, _)| w.eq_ignore_ascii_case(s))
        .map(|(_, n)| *n)
}

/// The disc number encoded in a directory name: `CD1` → 1, `Disc 2` → 2.
/// Returns `None` for names that merely start with the word, like `Discovery`.
pub fn disc_dir_number(name: &str) -> Option<u32> {
    let n = normalize(name);
    let rest = DISC_WORDS
        .iter()
        .find_map(|p| n.strip_prefix(p))?
        .trim_start_matches([' ', '-', '_', '.', '#']);
    parse_disc_number(rest)
}

/// True if a directory name looks like a disc subdirectory: `CD1`, `Disc 2`,
/// `disk-03`. These collapse into the parent so a multi-disc release stays one
/// album instead of fragmenting into one album per disc.
pub fn is_disc_dir(name: &str) -> bool {
    disc_dir_number(name).is_some()
}

/// The disc number of the disc subdirectory a file sits in, if any.
///
/// This is the fallback that matters most in practice: plenty of rips put each
/// disc in its own folder and never write a `disc` tag at all, which would
/// otherwise leave every track on "disc 1" with colliding track numbers.
pub fn disc_no_from_path(file: &Path) -> Option<u32> {
    file.parent()?
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(disc_dir_number)
}

/// Split a trailing disc marker off an album title.
///
/// `"Hullabaloo CD1"` → `("Hullabaloo", Some(1))`, and
/// `"Forty Licks ( Disc 2 )"` → `("Forty Licks", Some(2))`. Without this, the
/// two halves of a double album carry different album tags and get filed as two
/// separate releases.
pub fn strip_disc_suffix(title: &str) -> (String, Option<u32>) {
    let trimmed = title.trim();
    match split_disc_suffix(trimmed) {
        Some((head, n)) => (head, Some(n)),
        None => (trimmed.to_string(), None),
    }
}

fn split_disc_suffix(trimmed: &str) -> Option<(String, u32)> {
    let lower = trimmed.to_ascii_lowercase();

    // Take the *last* disc keyword sitting on a word boundary, so a title like
    // "Discovery Live CD2" resolves against the CD2 rather than "Discovery".
    let mut found: Option<(usize, usize)> = None;
    for keyword in DISC_WORDS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(keyword) {
            let start = from + rel;
            let end = start + keyword.len();
            let on_boundary = !lower[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
            if on_boundary && found.is_none_or(|(prev, _)| start > prev) {
                found = Some((start, end));
            }
            from = end;
        }
    }
    let (start, end) = found?;

    // Everything after the keyword must be exactly a number, allowing for
    // separators and a closing bracket. "( 2 Discs)" fails here, as it should.
    let tail = trimmed[end..]
        .trim()
        .trim_start_matches([' ', '-', '_', '.', '#', ':'])
        .trim_end_matches([')', ']', '}', ' ', '.'])
        .trim();
    let number = parse_disc_number(tail)?;

    let head = trimmed[..start]
        .trim_end_matches(|c: char| c.is_whitespace() || "-_.([{#:".contains(c))
        .trim();
    if head.is_empty() {
        return None; // The title was only ever "Disc 2"; leave it alone.
    }
    Some((head.to_string(), number))
}

/// The directory that represents the album a file belongs to. Normally the
/// file's parent, but hops up one level when the parent is a disc subdirectory.
pub fn album_dir(file: &Path) -> &Path {
    let parent = file.parent().unwrap_or(Path::new("."));
    let is_disc = parent
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_disc_dir);

    if is_disc {
        parent.parent().unwrap_or(parent)
    } else {
        parent
    }
}

/// Parse a leading integer out of tag values like `"3"`, `"03/12"`, or `"3 of 12"`.
pub fn leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Pull a four-digit year out of a date tag (`1997`, `1997-08-12`, `08/1997`).
pub fn year_from_date(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    bytes.windows(4).find_map(|w| {
        if !w.iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let y: i32 = std::str::from_utf8(w).ok()?.parse().ok()?;
        (1000..=2999).contains(&y).then_some(y)
    })
}

/// The most frequent non-empty value in an iterator, ties broken by first
/// occurrence. Used to pick one album artist / year / MBID for a whole album
/// when individual files disagree.
pub fn majority<I: IntoIterator<Item = Option<String>>>(values: I) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for v in values.into_iter().flatten() {
        if v.trim().is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(k, _)| *k == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_dirs_are_recognized() {
        for name in ["CD1", "cd 2", "Disc-3", "disk_04", "DISC 12"] {
            assert!(is_disc_dir(name), "{name} should be a disc dir");
        }
        for name in ["Discovery", "CD Singles", "Disc", "Abbey Road", "2 Discs"] {
            assert!(!is_disc_dir(name), "{name} should not be a disc dir");
        }
    }

    #[test]
    fn multi_disc_files_share_an_album_dir() {
        let a = album_dir(Path::new("/m/Pink Floyd/The Wall/CD1/01.flac"));
        let b = album_dir(Path::new("/m/Pink Floyd/The Wall/CD2/01.flac"));
        assert_eq!(a, b);
        assert_eq!(a, Path::new("/m/Pink Floyd/The Wall"));
    }

    #[test]
    fn single_disc_album_dir_is_the_parent() {
        let d = album_dir(Path::new("/m/Radiohead/Kid A/01.opus"));
        assert_eq!(d, Path::new("/m/Radiohead/Kid A"));
    }

    #[test]
    fn sort_key_moves_articles() {
        assert_eq!(sort_key("The Beatles"), "beatles");
        assert_eq!(sort_key("A Tribe Called Quest"), "tribe called quest");
        assert_eq!(sort_key("Anathema"), "anathema");
    }

    #[test]
    fn track_numbers_tolerate_totals() {
        assert_eq!(leading_u32("03/12"), Some(3));
        assert_eq!(leading_u32(" 7 "), Some(7));
        assert_eq!(leading_u32("A1"), None);
    }

    #[test]
    fn years_come_out_of_dates() {
        assert_eq!(year_from_date("1997-08-12"), Some(1997));
        assert_eq!(year_from_date("1997"), Some(1997));
        assert_eq!(year_from_date(""), None);
    }

    #[test]
    fn disc_numbers_come_out_of_directory_names() {
        assert_eq!(disc_dir_number("CD1"), Some(1));
        assert_eq!(disc_dir_number("CD 2"), Some(2));
        assert_eq!(disc_dir_number("Disc-03"), Some(3));
        assert_eq!(disc_dir_number("Disc Two"), Some(2));
        assert_eq!(disc_dir_number("Discovery"), None);
    }

    #[test]
    fn disc_number_is_inferred_from_the_containing_folder() {
        // The White Album case: disc folders, but no disc tag anywhere.
        let d1 = disc_no_from_path(Path::new("/m/Beatles/White Album/Disc 1/a.mp3"));
        let d2 = disc_no_from_path(Path::new("/m/Beatles/White Album/Disc 2/a.mp3"));
        assert_eq!((d1, d2), (Some(1), Some(2)));
        assert_eq!(disc_no_from_path(Path::new("/m/Radiohead/Kid A/01.opus")), None);
    }

    #[test]
    fn disc_markers_are_stripped_from_album_titles() {
        assert_eq!(strip_disc_suffix("Hullabaloo CD1"), ("Hullabaloo".into(), Some(1)));
        assert_eq!(
            strip_disc_suffix("Forty Licks ( Disc 2 )"),
            ("Forty Licks".into(), Some(2))
        );
        assert_eq!(
            strip_disc_suffix("Forty Licks (Disc One)"),
            ("Forty Licks".into(), Some(1))
        );
        assert_eq!(
            strip_disc_suffix("The Wall [CD 2]"),
            ("The Wall".into(), Some(2))
        );
    }

    #[test]
    fn ordinary_titles_survive_disc_stripping_untouched() {
        for title in [
            "Kid A",
            "Discovery",
            "Live at the BBC",
            "Rolling Stones - 40 Licks ( 2 Discs)",
            "Disc 2",
        ] {
            assert_eq!(strip_disc_suffix(title), (title.to_string(), None), "{title}");
        }
    }

    #[test]
    fn both_halves_of_a_split_double_reduce_to_one_title() {
        let (a, na) = strip_disc_suffix("Hullabaloo CD1");
        let (b, nb) = strip_disc_suffix("Hullabaloo CD2");
        assert_eq!(a, b);
        assert_ne!(na, nb);
    }

    #[test]
    fn majority_picks_the_common_value() {
        let vals = vec![
            Some("Miles Davis".into()),
            Some("Miles Davis".into()),
            Some("miles davis".into()),
            None,
        ];
        assert_eq!(majority(vals), Some("Miles Davis".into()));
    }
}
