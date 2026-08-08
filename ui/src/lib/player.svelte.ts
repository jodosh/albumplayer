/**
 * The browser player.
 *
 * Mirrors the album-first rules the Rust engine enforces: the queue holds
 * albums, shuffle reorders albums and never their tracks, and next-track and
 * next-album are separate controls.
 *
 * Two `<audio>` elements alternate so the next track is fully buffered before
 * the current one ends. That is as close to gapless as a browser gets without
 * Media Source Extensions; the desktop shell routes audio through GStreamer
 * instead and is genuinely gapless.
 */

import * as api from './api';
import * as native from './native';
import type { AlbumDetail, Track } from './api';

export type Repeat = 'off' | 'album' | 'queue';

/** A track counts as played past half its length, or four minutes. */
function isCompleted(msPlayed: number, durationMs: number): boolean {
  if (msPlayed <= 0) return false;
  const threshold = durationMs > 0 ? Math.min(durationMs / 2, 4 * 60_000) : 4 * 60_000;
  return msPlayed >= threshold;
}

class BrowserPlayer {
  /** Albums in the queue, in library order. */
  albums = $state<AlbumDetail[]>([]);
  /** Indices into `albums`, in play order. Shuffling permutes only this. */
  order = $state<number[]>([]);
  orderIndex = $state(0);
  trackIndex = $state(0);

  playing = $state(false);
  shuffle = $state(false);
  repeat = $state<Repeat>('off');
  positionMs = $state(0);
  durationMs = $state(0);
  volume = $state(1);

  /** The open album_session on the server, if any. */
  private sessionId: number | null = null;
  private sessionAlbumId: number | null = null;
  /** Furthest position reached in the current track, for the play log. */
  private heard = 0;

  private primary: HTMLAudioElement | null = null;
  private secondary: HTMLAudioElement | null = null;

  get album(): AlbumDetail | undefined {
    return this.albums[this.order[this.orderIndex]];
  }

  get track(): Track | undefined {
    return this.album?.tracks[this.trackIndex];
  }

  get upcoming(): AlbumDetail[] {
    return this.order.slice(this.orderIndex).map((i) => this.albums[i]);
  }

  /** Attach the audio elements once the DOM exists. */
  attach(primary: HTMLAudioElement, secondary: HTMLAudioElement) {
    this.primary = primary;
    this.secondary = secondary;

    for (const element of [primary, secondary]) {
      element.addEventListener('timeupdate', () => {
        if (element !== this.primary) return;
        this.positionMs = element.currentTime * 1000;
        this.heard = Math.max(this.heard, this.positionMs);
      });
      element.addEventListener('durationchange', () => {
        if (element === this.primary && Number.isFinite(element.duration)) {
          this.durationMs = element.duration * 1000;
        }
      });
      element.addEventListener('ended', () => {
        if (element === this.primary) void this.advance();
      });
      element.addEventListener('error', () => {
        // One unplayable file should not end the listening session.
        if (element === this.primary) void this.next();
      });
    }
  }

  /** Replace the queue with one album and start it. */
  async playAlbum(album: AlbumDetail, trackIndex = 0) {
    await this.closeSession();
    this.albums = [album];
    this.order = [0];
    this.orderIndex = 0;
    this.trackIndex = trackIndex;
    await this.start();
  }

  /** Append an album to the queue. */
  async enqueue(album: AlbumDetail) {
    const wasEmpty = this.albums.length === 0;
    this.albums = [...this.albums, album];
    this.order = [...this.order, this.albums.length - 1];
    if (wasEmpty) await this.start();
  }

  /** Queue many albums at once, optionally shuffled. */
  async playAll(albums: AlbumDetail[], shuffle = false) {
    if (albums.length === 0) return;
    await this.closeSession();
    this.albums = albums;
    this.order = albums.map((_, i) => i);
    if (shuffle) {
      this.shuffle = true;
      this.shuffleOrder(0);
    }
    this.orderIndex = 0;
    this.trackIndex = 0;
    await this.start();
  }

  /** Fisher-Yates over the not-yet-played part of the queue. */
  private shuffleOrder(from: number) {
    const next = [...this.order];
    for (let i = next.length - 1; i > from; i--) {
      const j = from + Math.floor(Math.random() * (i - from + 1));
      [next[i], next[j]] = [next[j], next[i]];
    }
    this.order = next;
  }

  setShuffle(on: boolean) {
    if (this.shuffle === on) return;
    this.shuffle = on;

    const currentAlbum = this.order[this.orderIndex];
    if (on) {
      // Nothing listened to yet means nothing to protect, so shuffle the lot —
      // otherwise turning shuffle on always starts you on the same album.
      const untouched = this.orderIndex === 0 && this.trackIndex === 0;
      this.shuffleOrder(untouched ? -1 : this.orderIndex);
      if (!untouched) {
        this.orderIndex = this.order.indexOf(currentAlbum);
      }
    } else {
      this.order = this.albums.map((_, i) => i);
      this.orderIndex = Math.max(0, this.order.indexOf(currentAlbum));
    }
  }

  /** Begin the track the cursor points at. */
  private async start() {
    const { album, track } = this;
    if (!album || !track || !this.primary) return;

    await this.openSessionFor(album.id);
    this.heard = 0;
    this.positionMs = 0;

    this.primary.src = api.streamUrl(track.id);
    this.applyGain();
    try {
      await this.primary.play();
      this.playing = true;
    } catch {
      // Autoplay policies can refuse until the user interacts; the transport
      // button will start it.
      this.playing = false;
    }
    this.preloadNext();
  }

  /** Warm the other element with the next track so the handover is quick. */
  private preloadNext() {
    if (!this.secondary) return;
    const next = this.peekNext();
    this.secondary.src = next ? api.streamUrl(next.id) : '';
    if (next) this.secondary.load();
  }

  private peekNext(): Track | undefined {
    const album = this.album;
    if (!album) return undefined;
    if (this.trackIndex + 1 < album.tracks.length) return album.tracks[this.trackIndex + 1];
    if (this.repeat === 'album') return album.tracks[0];
    if (this.orderIndex + 1 < this.order.length) {
      return this.albums[this.order[this.orderIndex + 1]]?.tracks[0];
    }
    if (this.repeat === 'queue') return this.albums[this.order[0]]?.tracks[0];
    return undefined;
  }

  /** A track finished on its own. */
  private async advance() {
    await this.reportPlay();
    const album = this.album;
    if (!album) return;

    if (this.trackIndex + 1 < album.tracks.length) {
      this.trackIndex += 1;
    } else if (this.repeat === 'album') {
      this.trackIndex = 0;
    } else if (this.orderIndex + 1 < this.order.length) {
      this.orderIndex += 1;
      this.trackIndex = 0;
    } else if (this.repeat === 'queue') {
      this.orderIndex = 0;
      this.trackIndex = 0;
    } else {
      await this.closeSession();
      this.playing = false;
      return;
    }
    await this.start();
  }

  /** Next track. Unlike a natural finish, this escapes a looping album. */
  async next() {
    await this.reportPlay();
    const album = this.album;
    if (album && this.trackIndex + 1 < album.tracks.length) {
      this.trackIndex += 1;
      await this.start();
    } else {
      await this.nextAlbum();
    }
  }

  async previous() {
    await this.reportPlay();
    if (this.trackIndex > 0) {
      this.trackIndex -= 1;
    } else if (this.orderIndex > 0) {
      this.orderIndex -= 1;
      this.trackIndex = Math.max(0, (this.album?.tracks.length ?? 1) - 1);
    }
    await this.start();
  }

  /** A separate control from next-track: in an album player these differ. */
  async nextAlbum() {
    await this.reportPlay();
    if (this.orderIndex + 1 < this.order.length) {
      this.orderIndex += 1;
    } else if (this.repeat === 'queue') {
      this.orderIndex = 0;
    } else {
      await this.closeSession();
      this.playing = false;
      return;
    }
    this.trackIndex = 0;
    await this.start();
  }

  /** Restart the current album, or step back to the previous one. */
  async previousAlbum() {
    await this.reportPlay();
    if (this.trackIndex === 0 && this.orderIndex > 0) this.orderIndex -= 1;
    this.trackIndex = 0;
    await this.start();
  }

  async toggle() {
    if (!this.primary || !this.track) return;
    if (this.playing) {
      this.primary.pause();
      this.playing = false;
    } else {
      await this.primary.play();
      this.playing = true;
    }
  }

  seek(ms: number) {
    if (this.primary) this.primary.currentTime = ms / 1000;
  }

  setVolume(value: number) {
    this.volume = Math.min(1, Math.max(0, value));
    this.applyGain();
  }

  /**
   * Apply album ReplayGain on top of the user's volume.
   *
   * Album gain, not track gain, and capped by the album's peak so boosting a
   * quiet record cannot clip it.
   */
  private applyGain() {
    if (!this.primary) return;
    const album = this.album;
    let scale = 1;
    if (album?.gain_db != null) {
      const linear = 10 ** (album.gain_db / 20);
      const ceiling = album.peak && album.peak > 0 ? 1 / album.peak : 4;
      scale = Math.min(linear, ceiling);
    }
    // An <audio> element cannot amplify past 1, so positive gain is limited to
    // avoiding attenuation rather than adding headroom.
    this.primary.volume = Math.min(1, Math.max(0, this.volume * scale));
  }

  private async openSessionFor(albumId: number) {
    if (this.sessionAlbumId === albumId && this.sessionId !== null) return;
    await this.closeSession();
    try {
      const { session_id } = await api.startSession(albumId);
      this.sessionId = session_id;
      this.sessionAlbumId = albumId;
    } catch {
      // Losing history is not worth interrupting playback for.
      this.sessionId = null;
      this.sessionAlbumId = albumId;
    }
  }

  private async closeSession() {
    if (this.sessionId === null) {
      this.sessionAlbumId = null;
      return;
    }
    const id = this.sessionId;
    this.sessionId = null;
    this.sessionAlbumId = null;
    try {
      await api.endSession(id);
    } catch {
      /* history is best-effort */
    }
  }

  /** Report how much of the current track was actually heard. */
  private async reportPlay() {
    const track = this.track;
    if (!track || this.heard <= 0) return;
    const heard = this.heard;
    this.heard = 0;
    try {
      await api.recordPlay(track.id, this.sessionId, Math.round(heard));
    } catch {
      /* best-effort */
    }
  }

  /** Settle the session when the page goes away. */
  async shutdown() {
    await this.reportPlay();
    await this.closeSession();
  }
}

/**
 * The desktop shell's player.
 *
 * The queue and the pipeline live in Rust; this class mirrors just enough state
 * for the interface to render, and keeps writing the play log to the server so
 * history stays in one place regardless of which client played the record.
 */
class NativePlayer {
  albums = $state<AlbumDetail[]>([]);
  playing = $state(false);
  shuffle = $state(false);
  repeat = $state<Repeat>('off');
  positionMs = $state(0);
  durationMs = $state(0);
  volume = $state(1);

  private currentAlbumId = $state<number | null>(null);
  private currentTrackId = $state<number | null>(null);
  private queuedCount = $state(0);

  private sessionId: number | null = null;
  private trackStartedAt = Math.floor(Date.now() / 1000);
  private started = false;

  get album(): AlbumDetail | undefined {
    return this.albums.find((a) => a.id === this.currentAlbumId);
  }

  get track(): Track | undefined {
    return this.album?.tracks.find((t) => t.id === this.currentTrackId);
  }

  get upcoming(): AlbumDetail[] {
    // Rust owns the real order; this is only used for the "n queued" hint.
    return this.albums.slice(0, Math.max(1, this.queuedCount));
  }

  /** Subscribe to engine events and start polling the transport clock. */
  attach() {
    if (this.started) return;
    this.started = true;

    void native.listen<{ album_id: number }>('album-started', async ({ album_id }) => {
      this.currentAlbumId = album_id;
      await this.openSession(album_id);
    });
    void native.listen<{ album_id: number; track_id: number }>(
      'track-started',
      ({ album_id, track_id }) => {
        this.currentAlbumId = album_id;
        this.currentTrackId = track_id;
        this.trackStartedAt = Math.floor(Date.now() / 1000);
      },
    );
    // The engine reports how much was actually audible; the server decides
    // whether that counts as a listen.
    void native.listen<{ track_id: number; ms_played: number }>(
      'track-finished',
      async ({ track_id, ms_played }) => {
        if (ms_played <= 0) return;
        try {
          await api.recordPlay(track_id, this.sessionId, Math.round(ms_played));
        } catch {
          /* history is best-effort */
        }
      },
    );
    void native.listen('queue-finished', async () => {
      this.playing = false;
      await this.closeSession();
    });
    void native.listen<{ message: string }>('player-error', ({ message }) =>
      console.error('player:', message),
    );

    setInterval(() => void this.poll(), 400);
  }

  private async poll() {
    try {
      const s = await native.invoke<native.NativeStatus>('status');
      this.playing = s.state === 'playing';
      this.positionMs = s.position_ms;
      this.durationMs = s.duration_ms;
      this.shuffle = s.shuffle;
      this.repeat = s.repeat;
      this.queuedCount = s.queued_albums;
      if (s.album_id !== null) this.currentAlbumId = s.album_id;
      if (s.track_id !== null) this.currentTrackId = s.track_id;
    } catch {
      /* the engine may not have started */
    }
  }

  /** Shape an album for the shell, with stream URLs it can hand to GStreamer. */
  private wire(album: AlbumDetail) {
    return {
      id: album.id,
      title: album.title,
      artist: album.artist,
      gain_db: album.gain_db,
      peak: album.peak,
      tracks: album.tracks.map((t) => ({
        id: t.id,
        url: api.streamUrl(t.id),
        title: t.title,
        disc_no: t.disc_no,
        track_no: t.track_no,
        duration_ms: t.duration_ms,
      })),
    };
  }

  private remember(albums: AlbumDetail[]) {
    const byId = new Map(this.albums.map((a) => [a.id, a]));
    for (const album of albums) byId.set(album.id, album);
    this.albums = [...byId.values()];
  }

  async playAlbum(album: AlbumDetail, trackIndex = 0) {
    this.remember([album]);
    await native.invoke('play_album', { album: this.wire(album) });
    // The engine always starts at track one; jumping is a separate step.
    for (let i = 0; i < trackIndex; i++) await native.invoke('next_track');
  }

  async enqueue(album: AlbumDetail) {
    this.remember([album]);
    await native.invoke('enqueue', { album: this.wire(album) });
  }

  async playAll(albums: AlbumDetail[], shuffle = false) {
    if (albums.length === 0) return;
    this.remember(albums);
    const [first, ...rest] = albums;
    await native.invoke('play_album', { album: this.wire(first) });
    for (const album of rest) await native.invoke('enqueue', { album: this.wire(album) });
    if (shuffle) await this.setShuffle(true);
  }

  async setShuffle(on: boolean) {
    this.shuffle = on;
    await native.invoke('set_shuffle', { on });
  }

  async setRepeat(mode: Repeat) {
    this.repeat = mode;
    await native.invoke('set_repeat', { mode });
  }

  next = () => native.invoke('next_track');
  previous = () => native.invoke('prev_track');
  nextAlbum = () => native.invoke('next_album');
  previousAlbum = () => native.invoke('prev_album');
  toggle = () => native.invoke('toggle_pause');
  seek = (ms: number) => void native.invoke('seek', { positionMs: Math.round(ms) });

  setVolume(value: number) {
    this.volume = Math.min(1, Math.max(0, value));
    void native.invoke('set_volume', { volume: this.volume });
  }

  private async openSession(albumId: number) {
    await this.closeSession();
    try {
      const { session_id } = await api.startSession(albumId);
      this.sessionId = session_id;
    } catch {
      this.sessionId = null;
    }
  }

  private async closeSession() {
    if (this.sessionId === null) return;
    const id = this.sessionId;
    this.sessionId = null;
    try {
      await api.endSession(id);
    } catch {
      /* best-effort */
    }
  }

  async shutdown() {
    await native.invoke('stop').catch(() => {});
    await this.closeSession();
  }
}

/** True when the desktop shell is providing gapless native playback. */
export const nativePlayback = native.isNative();

/**
 * Whichever player this build is running under. The components above do not
 * care which: both expose the same album-first controls.
 */
export const player = nativePlayback ? new NativePlayer() : new BrowserPlayer();

export { isCompleted };
