//! The play queue.
//!
//! This is where "album-first" stops being a slogan and becomes behaviour. The
//! queue holds *albums*, not a flat list of tracks:
//!
//! * Shuffle reorders albums and never touches the order of tracks inside one.
//! * Skipping forward has two distinct meanings — next track and next album —
//!   which most players conflate into a single control.
//! * Repeat can apply to the current album or to the whole queue.
//!
//! The model is deliberately free of GStreamer so that all of it can be tested
//! without an audio device.

use std::path::PathBuf;

/// Where a track's audio comes from.
///
/// The desktop app streams from the server rather than reading the share
/// directly, so the engine has to accept both. GStreamer plays an `https://`
/// URI as readily as a local file, which is what keeps gapless working when the
/// music lives in a homelab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Url(String),
}

impl Source {
    /// The URI to hand to the pipeline.
    pub fn to_uri(&self) -> String {
        match self {
            Self::File(path) => gstreamer::glib::filename_to_uri(path, None)
                .map(|u| u.to_string())
                .unwrap_or_else(|_| format!("file://{}", path.display())),
            Self::Url(url) => url.clone(),
        }
    }
}

/// One track, as the player needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTrack {
    pub id: i64,
    pub source: Source,
    pub title: String,
    pub disc_no: i64,
    pub track_no: i64,
    pub duration_ms: i64,
}

/// One album's worth of tracks, in playback order.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedAlbum {
    pub id: i64,
    pub title: String,
    pub artist: String,
    /// Album ReplayGain in dB. Album gain, never track gain — track gain would
    /// flatten the dynamics the record was mastered with.
    pub album_gain_db: Option<f64>,
    pub album_peak: Option<f64>,
    pub tracks: Vec<QueuedTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    /// Loop the current album forever.
    Album,
    /// Wrap around to the first album when the queue runs out.
    Queue,
}

/// Where playback currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Index into the play order, not into `albums`.
    pub order_index: usize,
    pub track_index: usize,
}

/// A queue of albums with a cursor into it.
#[derive(Debug, Clone, Default)]
pub struct AlbumQueue {
    albums: Vec<QueuedAlbum>,
    /// Indices into `albums`, in the order they will play. Shuffling permutes
    /// this and leaves `albums` — and every album's track order — untouched.
    order: Vec<usize>,
    cursor: Option<Cursor>,
    shuffle: bool,
    repeat: Repeat,
    /// Seed for the shuffle, kept so reshuffles are reproducible in tests.
    seed: u64,
}

impl AlbumQueue {
    pub fn new() -> Self {
        // Seeded from the clock, or shuffle would deal the same order on every
        // run — which is not shuffle.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545_F491_4F6C_DD1D);
        Self {
            seed: seed | 1,
            ..Default::default()
        }
    }

    /// Replace the queue with a single album and start at its first track.
    pub fn play_album(&mut self, album: QueuedAlbum) {
        self.albums.clear();
        self.order.clear();
        self.cursor = None;
        self.enqueue(album);
        if !self.is_empty() {
            self.cursor = Some(Cursor {
                order_index: 0,
                track_index: 0,
            });
        }
    }

    /// Append an album to the end of the queue.
    pub fn enqueue(&mut self, album: QueuedAlbum) {
        if album.tracks.is_empty() {
            return;
        }
        self.albums.push(album);
        let index = self.albums.len() - 1;

        // A shuffled queue drops new arrivals at a random point among the
        // albums not yet played, so enqueueing mid-listen does not always
        // append to the very end.
        if self.shuffle && self.cursor.is_some() {
            let first_unplayed = self.cursor.map_or(0, |c| c.order_index + 1);
            let span = self.order.len().saturating_sub(first_unplayed);
            let at = first_unplayed + if span == 0 { 0 } else { self.next_rand() as usize % (span + 1) };
            self.order.insert(at, index);
        } else {
            self.order.push(index);
        }

        if self.cursor.is_none() {
            self.cursor = Some(Cursor {
                order_index: 0,
                track_index: 0,
            });
        }
    }

    pub fn clear(&mut self) {
        self.albums.clear();
        self.order.clear();
        self.cursor = None;
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn album_count(&self) -> usize {
        self.order.len()
    }

    pub fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    pub fn shuffle_enabled(&self) -> bool {
        self.shuffle
    }

    /// Turn album shuffle on or off.
    ///
    /// Enabling reorders the albums that have not played yet and leaves the
    /// current one where it is, so the record you are listening to is not
    /// yanked out from under you. Disabling restores release order.
    pub fn set_shuffle(&mut self, shuffle: bool) {
        if self.shuffle == shuffle {
            return;
        }
        self.shuffle = shuffle;

        // Nothing has been listened to yet if we are still sitting on the very
        // first track of the very first album. In that case there is no record
        // "in progress" to protect, so the whole queue gets shuffled — otherwise
        // turning on shuffle before pressing play would always start you on the
        // same album.
        let untouched = self.cursor == Some(Cursor {
            order_index: 0,
            track_index: 0,
        });
        let pinned = if untouched {
            None
        } else {
            self.cursor.map(|c| self.order[c.order_index])
        };

        if shuffle {
            let from = match self.cursor {
                Some(c) if pinned.is_some() => c.order_index + 1,
                _ => 0,
            };
            self.shuffle_tail(from);
        } else {
            self.order = (0..self.albums.len()).collect();
        }

        // Keep the cursor pointed at the same album after the permutation.
        if let (Some(album), Some(cursor)) = (pinned, self.cursor.as_mut())
            && let Some(new_index) = self.order.iter().position(|&i| i == album)
        {
            cursor.order_index = new_index;
        }
    }

    /// Fisher-Yates over `order[from..]`, using an internal PRNG so that tests
    /// are deterministic and no external dependency is needed.
    fn shuffle_tail(&mut self, from: usize) {
        if from >= self.order.len() {
            return;
        }
        for i in (from + 1..self.order.len()).rev() {
            let j = from + (self.next_rand() as usize % (i - from + 1));
            self.order.swap(i, j);
        }
    }

    /// xorshift64*, adequate for shuffling a few hundred albums.
    fn next_rand(&mut self) -> u64 {
        let mut x = self.seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.seed = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Seed the shuffle. Only useful for reproducible tests.
    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed | 1;
    }

    pub fn current_album(&self) -> Option<&QueuedAlbum> {
        let cursor = self.cursor?;
        self.albums.get(*self.order.get(cursor.order_index)?)
    }

    pub fn current_track(&self) -> Option<&QueuedTrack> {
        let cursor = self.cursor?;
        self.current_album()?.tracks.get(cursor.track_index)
    }

    /// The track that will play after this one, without moving the cursor.
    ///
    /// This is what makes gapless playback possible: the next file has to be
    /// handed to the pipeline before the current one finishes.
    pub fn peek_next(&self) -> Option<&QueuedTrack> {
        let next = self.next_cursor()?;
        let album = self.albums.get(*self.order.get(next.order_index)?)?;
        album.tracks.get(next.track_index)
    }

    /// Where the cursor would land after the current track finishes naturally.
    fn next_cursor(&self) -> Option<Cursor> {
        let cursor = self.cursor?;
        let album = self.current_album()?;

        if cursor.track_index + 1 < album.tracks.len() {
            return Some(Cursor {
                track_index: cursor.track_index + 1,
                ..cursor
            });
        }

        // End of the album.
        match self.repeat {
            Repeat::Album => Some(Cursor {
                order_index: cursor.order_index,
                track_index: 0,
            }),
            _ if cursor.order_index + 1 < self.order.len() => Some(Cursor {
                order_index: cursor.order_index + 1,
                track_index: 0,
            }),
            Repeat::Queue if !self.order.is_empty() => Some(Cursor {
                order_index: 0,
                track_index: 0,
            }),
            _ => None,
        }
    }

    /// Advance as if the current track played to its end.
    ///
    /// Returns false when the queue is exhausted, which is the signal to stop.
    pub fn advance(&mut self) -> bool {
        match self.next_cursor() {
            Some(next) => {
                self.cursor = Some(next);
                true
            }
            None => false,
        }
    }

    /// User pressed next-track. Same movement as a natural finish, except that
    /// it ignores `Repeat::Album` — asking for the next track at the end of a
    /// looping album should move on, not restart it.
    pub fn next_track(&mut self) -> bool {
        let Some(cursor) = self.cursor else {
            return false;
        };
        let Some(album) = self.current_album() else {
            return false;
        };

        if cursor.track_index + 1 < album.tracks.len() {
            self.cursor = Some(Cursor {
                track_index: cursor.track_index + 1,
                ..cursor
            });
            return true;
        }
        self.next_album()
    }

    /// Step back one track, crossing into the previous album's last track.
    pub fn prev_track(&mut self) -> bool {
        let Some(cursor) = self.cursor else {
            return false;
        };

        if cursor.track_index > 0 {
            self.cursor = Some(Cursor {
                track_index: cursor.track_index - 1,
                ..cursor
            });
            return true;
        }
        if cursor.order_index == 0 {
            return false;
        }

        let prev_index = cursor.order_index - 1;
        let last = self
            .albums
            .get(self.order[prev_index])
            .map_or(0, |a| a.tracks.len().saturating_sub(1));
        self.cursor = Some(Cursor {
            order_index: prev_index,
            track_index: last,
        });
        true
    }

    /// Jump to the start of the next album. A separate control from next-track
    /// on purpose — in an album-first player these are different intentions.
    pub fn next_album(&mut self) -> bool {
        let Some(cursor) = self.cursor else {
            return false;
        };
        if cursor.order_index + 1 < self.order.len() {
            self.cursor = Some(Cursor {
                order_index: cursor.order_index + 1,
                track_index: 0,
            });
            return true;
        }
        if self.repeat == Repeat::Queue && !self.order.is_empty() {
            self.cursor = Some(Cursor {
                order_index: 0,
                track_index: 0,
            });
            return true;
        }
        false
    }

    /// Jump to the start of the previous album, or restart the current one if
    /// we are already partway into it.
    pub fn prev_album(&mut self) -> bool {
        let Some(cursor) = self.cursor else {
            return false;
        };
        if cursor.track_index > 0 {
            self.cursor = Some(Cursor {
                track_index: 0,
                ..cursor
            });
            return true;
        }
        if cursor.order_index == 0 {
            return false;
        }
        self.cursor = Some(Cursor {
            order_index: cursor.order_index - 1,
            track_index: 0,
        });
        true
    }

    /// Jump directly to a track within the current album.
    pub fn seek_to_track(&mut self, track_index: usize) -> bool {
        let Some(cursor) = self.cursor else {
            return false;
        };
        let Some(album) = self.current_album() else {
            return false;
        };
        if track_index >= album.tracks.len() {
            return false;
        }
        self.cursor = Some(Cursor {
            track_index,
            ..cursor
        });
        true
    }

    /// The albums in play order, for showing "up next".
    pub fn upcoming(&self) -> impl Iterator<Item = &QueuedAlbum> {
        let from = self.cursor.map_or(0, |c| c.order_index);
        self.order[from..]
            .iter()
            .filter_map(move |&i| self.albums.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: i64, tracks: usize) -> QueuedAlbum {
        QueuedAlbum {
            id,
            title: format!("Album {id}"),
            artist: "Artist".into(),
            album_gain_db: None,
            album_peak: None,
            tracks: (0..tracks)
                .map(|i| QueuedTrack {
                    id: id * 100 + i as i64,
                    source: Source::File(PathBuf::from(format!("/m/{id}/{i}.opus"))),
                    title: format!("Track {i}"),
                    disc_no: 1,
                    track_no: i as i64 + 1,
                    duration_ms: 180_000,
                })
                .collect(),
        }
    }

    fn queue_of(counts: &[usize]) -> AlbumQueue {
        let mut q = AlbumQueue::new();
        for (i, &n) in counts.iter().enumerate() {
            q.enqueue(album(i as i64 + 1, n));
        }
        q
    }

    #[test]
    fn playing_an_album_starts_at_its_first_track() {
        let mut q = AlbumQueue::new();
        q.play_album(album(1, 5));
        assert_eq!(q.current_track().unwrap().track_no, 1);
        assert_eq!(q.current_album().unwrap().id, 1);
    }

    #[test]
    fn advancing_runs_an_album_in_order_then_moves_to_the_next() {
        let mut q = queue_of(&[3, 2]);
        let mut seen = Vec::new();
        loop {
            let t = q.current_track().unwrap();
            seen.push((q.current_album().unwrap().id, t.track_no));
            if !q.advance() {
                break;
            }
        }
        assert_eq!(seen, vec![(1, 1), (1, 2), (1, 3), (2, 1), (2, 2)]);
    }

    #[test]
    fn empty_albums_are_never_enqueued() {
        let mut q = AlbumQueue::new();
        q.enqueue(album(1, 0));
        assert!(q.is_empty());
        assert!(q.current_track().is_none());
    }

    #[test]
    fn peek_next_crosses_the_album_boundary() {
        let mut q = queue_of(&[1, 2]);
        // Sitting on the only track of album 1; the next file is album 2's first.
        assert_eq!(q.peek_next().unwrap().id, 200);
        q.advance();
        assert_eq!(q.current_album().unwrap().id, 2);
    }

    #[test]
    fn peek_next_is_none_at_the_very_end() {
        let mut q = queue_of(&[1]);
        assert!(q.peek_next().is_none());
        assert!(!q.advance());
    }

    #[test]
    fn shuffle_reorders_albums_but_never_their_tracks() {
        let mut q = queue_of(&[4, 4, 4, 4, 4, 4, 4, 4]);
        q.set_seed(12345);
        q.set_shuffle(true);

        // Walk the whole queue and confirm every album's tracks stayed in order.
        let mut per_album: Vec<(i64, Vec<i64>)> = Vec::new();
        loop {
            let album_id = q.current_album().unwrap().id;
            let track_no = q.current_track().unwrap().track_no;
            match per_album.last_mut() {
                Some((id, nos)) if *id == album_id => nos.push(track_no),
                _ => per_album.push((album_id, vec![track_no])),
            }
            if !q.advance() {
                break;
            }
        }

        assert_eq!(per_album.len(), 8, "each album played as one contiguous run");
        for (_, track_nos) in &per_album {
            assert_eq!(*track_nos, vec![1, 2, 3, 4], "track order is never shuffled");
        }

        let order: Vec<i64> = per_album.iter().map(|(id, _)| *id).collect();
        assert_ne!(order, vec![1, 2, 3, 4, 5, 6, 7, 8], "album order did change");
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![1, 2, 3, 4, 5, 6, 7, 8], "no album lost or repeated");
    }

    #[test]
    fn enabling_shuffle_keeps_you_on_the_current_album() {
        let mut q = queue_of(&[3, 3, 3, 3, 3]);
        q.next_album();
        q.next_track();
        let (album_id, track_no) = (
            q.current_album().unwrap().id,
            q.current_track().unwrap().track_no,
        );

        q.set_seed(999);
        q.set_shuffle(true);

        assert_eq!(q.current_album().unwrap().id, album_id);
        assert_eq!(q.current_track().unwrap().track_no, track_no);
    }

    #[test]
    fn shuffling_before_playback_can_start_on_any_album() {
        // Turning shuffle on before pressing play must be able to land on an
        // album other than the first, or every session starts the same way.
        let mut first_albums = std::collections::HashSet::new();
        for seed in 1..40u64 {
            let mut q = queue_of(&[1, 1, 1, 1, 1, 1, 1, 1]);
            q.set_seed(seed);
            q.set_shuffle(true);
            first_albums.insert(q.current_album().unwrap().id);
        }
        assert!(
            first_albums.len() > 1,
            "shuffle always started on album {first_albums:?}"
        );
    }

    #[test]
    fn two_queues_do_not_shuffle_identically() {
        // A fixed seed would deal the same order every run.
        let orders: Vec<Vec<i64>> = (0..2)
            .map(|_| {
                let mut q = queue_of(&[1; 12]);
                q.set_shuffle(true);
                q.upcoming().map(|a| a.id).collect()
            })
            .collect();
        assert_ne!(orders[0], orders[1], "shuffle is seeded from the clock");
    }

    #[test]
    fn disabling_shuffle_restores_release_order() {
        let mut q = queue_of(&[2, 2, 2, 2]);
        q.set_seed(7);
        q.set_shuffle(true);
        q.set_shuffle(false);

        let ids: Vec<i64> = q.upcoming().map(|a| a.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn next_track_and_next_album_are_different_controls() {
        let mut q = queue_of(&[5, 5]);

        q.next_track();
        assert_eq!(q.current_album().unwrap().id, 1);
        assert_eq!(q.current_track().unwrap().track_no, 2);

        q.next_album();
        assert_eq!(q.current_album().unwrap().id, 2);
        assert_eq!(q.current_track().unwrap().track_no, 1, "lands at the start");
    }

    #[test]
    fn next_track_at_the_end_of_an_album_moves_on() {
        let mut q = queue_of(&[2, 2]);
        q.next_track();
        assert!(q.next_track());
        assert_eq!(q.current_album().unwrap().id, 2);
        assert_eq!(q.current_track().unwrap().track_no, 1);
    }

    #[test]
    fn prev_track_steps_back_into_the_previous_album() {
        let mut q = queue_of(&[3, 3]);
        q.next_album();
        assert!(q.prev_track());
        assert_eq!(q.current_album().unwrap().id, 1);
        assert_eq!(q.current_track().unwrap().track_no, 3, "its last track");
    }

    #[test]
    fn prev_album_restarts_before_it_goes_back() {
        let mut q = queue_of(&[3, 3]);
        q.next_album();
        q.next_track();

        assert!(q.prev_album());
        assert_eq!(q.current_album().unwrap().id, 2, "restarts album 2 first");
        assert_eq!(q.current_track().unwrap().track_no, 1);

        assert!(q.prev_album());
        assert_eq!(q.current_album().unwrap().id, 1);
    }

    #[test]
    fn you_cannot_step_off_the_front_of_the_queue() {
        let mut q = queue_of(&[2]);
        assert!(!q.prev_track());
        assert!(!q.prev_album());
        assert_eq!(q.current_track().unwrap().track_no, 1);
    }

    #[test]
    fn repeat_album_loops_the_record() {
        let mut q = queue_of(&[2, 2]);
        q.set_repeat(Repeat::Album);
        q.advance();
        assert!(q.advance(), "does not run off the end");
        assert_eq!(q.current_album().unwrap().id, 1);
        assert_eq!(q.current_track().unwrap().track_no, 1);
    }

    #[test]
    fn next_track_escapes_a_looping_album() {
        // Repeat::Album should not trap a deliberate skip.
        let mut q = queue_of(&[2, 2]);
        q.set_repeat(Repeat::Album);
        q.next_track();
        assert!(q.next_track());
        assert_eq!(q.current_album().unwrap().id, 2);
    }

    #[test]
    fn repeat_queue_wraps_to_the_first_album() {
        let mut q = queue_of(&[1, 1]);
        q.set_repeat(Repeat::Queue);
        q.advance();
        assert_eq!(q.current_album().unwrap().id, 2);
        assert!(q.advance());
        assert_eq!(q.current_album().unwrap().id, 1);
    }

    #[test]
    fn repeat_off_stops_at_the_end() {
        let mut q = queue_of(&[1, 1]);
        q.advance();
        assert!(!q.advance());
    }

    #[test]
    fn seeking_within_an_album_is_bounded() {
        let mut q = queue_of(&[4]);
        assert!(q.seek_to_track(3));
        assert_eq!(q.current_track().unwrap().track_no, 4);
        assert!(!q.seek_to_track(4), "out of range is refused");
        assert_eq!(q.current_track().unwrap().track_no, 4, "cursor unmoved");
    }

    #[test]
    fn upcoming_starts_with_the_album_now_playing() {
        let mut q = queue_of(&[2, 2, 2]);
        q.next_album();
        let ids: Vec<i64> = q.upcoming().map(|a| a.id).collect();
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn play_album_replaces_whatever_was_queued() {
        let mut q = queue_of(&[2, 2, 2]);
        q.next_album();
        q.play_album(album(99, 3));
        assert_eq!(q.album_count(), 1);
        assert_eq!(q.current_album().unwrap().id, 99);
        assert_eq!(q.current_track().unwrap().track_no, 1);
    }
}
