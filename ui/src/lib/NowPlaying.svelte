<script lang="ts">
  import { player } from './player.svelte';
  import Cover from './Cover.svelte';
  import Icon from './Icon.svelte';
  import { formatDuration } from './format';

  let album = $derived(player.album);
  let track = $derived(player.track);
  let progress = $derived(
    player.durationMs > 0 ? (player.positionMs / player.durationMs) * 100 : 0,
  );

  function scrub(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    player.seek((Number(input.value) / 100) * player.durationMs);
  }

  function cycleRepeat() {
    const next = player.repeat === 'off' ? 'album' : player.repeat === 'album' ? 'queue' : 'off';
    // The native player forwards this to the engine; the browser one just sets
    // the field. `setRepeat` exists on both.
    if ('setRepeat' in player) void (player as { setRepeat: (m: typeof next) => void }).setRepeat(next);
    else player.repeat = next;
  }
</script>

{#if album && track}
  <footer>
    <div class="art">
      <Cover
        albumId={album.id}
        title={album.title}
        artist={album.artist}
        hasCover={album.has_cover}
      />
    </div>

    <div class="what">
      <div class="track">{track.title}</div>
      <div class="album">{album.artist} — {album.title}</div>
    </div>

    <div class="transport">
      <!-- Album and track skips are deliberately separate controls. -->
      <button onclick={() => player.previousAlbum()} title="Previous album" aria-label="Previous album">
        <Icon name="prev-album" />
      </button>
      <button onclick={() => player.previous()} title="Previous track" aria-label="Previous track">
        <Icon name="prev" />
      </button>
      <button
        class="play"
        onclick={() => player.toggle()}
        title={player.playing ? 'Pause' : 'Play'}
        aria-label={player.playing ? 'Pause' : 'Play'}
      >
        <Icon name={player.playing ? 'pause' : 'play'} size={22} />
      </button>
      <button onclick={() => player.next()} title="Next track" aria-label="Next track">
        <Icon name="next" />
      </button>
      <button onclick={() => player.nextAlbum()} title="Next album" aria-label="Next album">
        <Icon name="next-album" />
      </button>
    </div>

    <div class="scrub">
      <span class="time">{formatDuration(player.positionMs)}</span>
      <input
        type="range"
        min="0"
        max="100"
        step="0.1"
        value={progress}
        oninput={scrub}
        aria-label="Seek"
      />
      <span class="time">{formatDuration(player.durationMs)}</span>
    </div>

    <div class="modes">
      <button
        class:on={player.shuffle}
        onclick={() => player.setShuffle(!player.shuffle)}
        title="Shuffle albums (track order is kept)"
        aria-label="Shuffle albums"
      >
        <Icon name="shuffle" size={16} />
      </button>
      <button
        class:on={player.repeat !== 'off'}
        onclick={cycleRepeat}
        title="Repeat: {player.repeat}"
        aria-label="Repeat: {player.repeat}"
      >
        <Icon name={player.repeat === 'album' ? 'repeat-one' : 'repeat'} size={16} />
      </button>
      {#if player.upcoming.length > 1}
        <span class="queued" title="Albums still to play">{player.upcoming.length - 1} queued</span>
      {/if}
    </div>
  </footer>
{/if}

<style>
  footer {
    position: fixed;
    inset: auto 0 0 0;
    display: grid;
    grid-template-columns: auto minmax(8rem, 1fr) auto minmax(10rem, 1.4fr) auto;
    align-items: center;
    gap: 1rem;
    padding: 0.7rem 1.25rem;
    background: var(--bar);
    border-top: 1px solid var(--line);
    backdrop-filter: blur(12px);
  }
  .art {
    width: 3rem;
  }
  .what {
    min-width: 0;
  }
  .track,
  .album {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .track {
    font-size: 0.9rem;
  }
  .album {
    font-size: 0.75rem;
    color: var(--dim);
  }
  .transport,
  .modes {
    display: flex;
    align-items: center;
    gap: 0.15rem;
  }
  button {
    border: 0;
    background: none;
    color: var(--fg);
    cursor: pointer;
    font-size: 0.85rem;
    padding: 0.35rem 0.45rem;
    border-radius: 6px;
    line-height: 1;
  }
  button:hover {
    background: var(--raised);
  }
  .play {
    padding: 0.4rem 0.5rem;
  }
  .modes button {
    opacity: 0.45;
  }
  .modes button.on {
    opacity: 1;
    color: var(--accent);
  }
  .scrub {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }
  .time {
    font-size: 0.7rem;
    color: var(--dim);
    font-variant-numeric: tabular-nums;
    min-width: 2.6rem;
  }
  .time:last-of-type {
    text-align: right;
  }
  input[type='range'] {
    flex: 1;
    accent-color: var(--accent);
  }
  .queued {
    font-size: 0.7rem;
    color: var(--dim);
    white-space: nowrap;
  }

  @media (max-width: 46rem) {
    footer {
      grid-template-columns: auto 1fr auto;
      gap: 0.6rem;
    }
    .scrub,
    .modes {
      display: none;
    }
  }
</style>
