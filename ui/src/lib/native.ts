/**
 * Bridge to the desktop shell.
 *
 * The same bundle runs in a browser and inside Tauri. In the browser these
 * helpers report unavailable and the UI falls back to `<audio>`; inside the
 * shell they hand playback to the Rust engine, which is genuinely gapless.
 *
 * Uses the `window.__TAURI__` global rather than the npm package so the browser
 * build carries no extra dependency.
 */

interface TauriGlobal {
  core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
  event: {
    listen<T>(name: string, handler: (event: { payload: T }) => void): Promise<() => void>;
  };
}

function tauri(): TauriGlobal | null {
  const global = window as unknown as { __TAURI__?: TauriGlobal };
  return global.__TAURI__ ?? null;
}

/** True when running inside the desktop shell. */
export const isNative = (): boolean => tauri() !== null;

export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const api = tauri();
  if (!api) throw new Error('not running in the desktop shell');
  return api.core.invoke<T>(cmd, args);
}

export async function listen<T>(
  name: string,
  handler: (payload: T) => void,
): Promise<() => void> {
  const api = tauri();
  if (!api) return () => {};
  return api.event.listen<T>(name, (event) => handler(event.payload));
}

export interface NativeStatus {
  state: 'playing' | 'paused' | 'stopped';
  album_id: number | null;
  track_id: number | null;
  track_title: string | null;
  album_title: string | null;
  artist: string | null;
  position_ms: number;
  duration_ms: number;
  shuffle: boolean;
  repeat: 'off' | 'album' | 'queue';
  queued_albums: number;
}
