/**
 * Client for the album server.
 *
 * The base URL is configurable because the same bundle is served two ways: from
 * the server itself (same origin), and from the desktop shell, which points at
 * a homelab address. Both authenticate with the same bearer token.
 */

// localStorage key names, not secrets. The session token itself is issued by
// the server at login and only ever lives in browser storage.
const TOKEN_STORAGE_KEY = 'albumplayer.token';
const BASE_STORAGE_KEY = 'albumplayer.base';

export interface AlbumSummary {
  id: number;
  title: string;
  artist: string;
  year: number | null;
  track_count: number;
  disc_count: number;
  duration_ms: number;
  is_compilation: boolean;
  play_count: number;
  has_cover: boolean;
}

export interface Track {
  id: number;
  disc_no: number;
  track_no: number;
  title: string;
  artist: string;
  duration_ms: number;
  codec: string | null;
}

export interface AlbumDetail extends AlbumSummary {
  gain_db: number | null;
  peak: number | null;
  tracks: Track[];
}

export interface Stats {
  albums: number;
  artists: number;
  tracks: number;
  total_duration_ms: number;
  album_plays: number;
  track_plays: number;
}

/** Thrown when the server rejects our token, so the UI can show the login form. */
export class Unauthorized extends Error {}

/**
 * Where the server lives.
 *
 * Served from the server itself this is just the current origin. Inside the
 * desktop shell the origin is `tauri://localhost`, which is not a server at
 * all, so an address has to be configured and there is no useful default.
 */
function baseUrl(): string {
  const stored = localStorage.getItem(BASE_STORAGE_KEY);
  if (stored) return stored;
  const origin = window.location.origin;
  return origin.startsWith('http') ? origin : '';
}

/** True when no server address is known and the user must supply one. */
export function needsServerAddress(): boolean {
  return baseUrl() === '';
}

export function setBaseUrl(url: string) {
  localStorage.setItem(BASE_STORAGE_KEY, url.replace(/\/$/, ''));
}

export function token(): string | null {
  return localStorage.getItem(TOKEN_STORAGE_KEY);
}

export function clearToken() {
  localStorage.removeItem(TOKEN_STORAGE_KEY);
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  const t = token();
  if (t) headers.set('Authorization', `Bearer ${t}`);
  if (init.body) headers.set('Content-Type', 'application/json');

  const response = await fetch(`${baseUrl()}${path}`, { ...init, headers });
  if (response.status === 401) {
    clearToken();
    throw new Unauthorized('session expired');
  }
  if (!response.ok) {
    const detail = await response.json().catch(() => ({ error: response.statusText }));
    throw new Error(detail.error ?? `request failed: ${response.status}`);
  }
  return response.json() as Promise<T>;
}

export async function login(password: string): Promise<void> {
  // Deliberately not using `request`: there is no token yet, and a wrong
  // password must surface as a message rather than a redirect loop.
  const response = await fetch(`${baseUrl()}/api/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ password }),
  });
  if (response.status === 401) throw new Error('Wrong password');
  if (!response.ok) throw new Error(`Could not reach the server (${response.status})`);

  const body = (await response.json()) as { token: string };
  localStorage.setItem(TOKEN_STORAGE_KEY, body.token);
}

export const albums = (params: { sort?: string; search?: string; limit?: number } = {}) => {
  const query = new URLSearchParams();
  if (params.sort) query.set('sort', params.sort);
  if (params.search) query.set('search', params.search);
  query.set('limit', String(params.limit ?? 5000));
  return request<AlbumSummary[]>(`/api/albums?${query}`);
};

export const album = (id: number) => request<AlbumDetail>(`/api/albums/${id}`);
export const stats = () => request<Stats>('/api/stats');

/**
 * Media URLs carry the token in the query string: `<img src>` and `<audio src>`
 * cannot set an Authorization header.
 */
export const coverUrl = (albumId: number) =>
  `${baseUrl()}/api/albums/${albumId}/cover?token=${encodeURIComponent(token() ?? '')}`;

export const streamUrl = (trackId: number) =>
  `${baseUrl()}/api/tracks/${trackId}/stream?token=${encodeURIComponent(token() ?? '')}`;

export const startSession = (albumId: number) =>
  request<{ session_id: number }>('/api/sessions', {
    method: 'POST',
    body: JSON.stringify({ album_id: albumId }),
  });

export const endSession = (sessionId: number) =>
  request<{ finished: boolean }>(`/api/sessions/${sessionId}/end`, { method: 'POST' });

export const recordPlay = (trackId: number, sessionId: number | null, msPlayed: number) =>
  request<{ completed: boolean }>('/api/plays', {
    method: 'POST',
    body: JSON.stringify({ track_id: trackId, session_id: sessionId, ms_played: msPlayed }),
  });
