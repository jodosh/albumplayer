<script lang="ts">
  import * as api from './api';
  import type { AlbumDetail } from './api';
  import { player } from './player.svelte';
  import Cover from './Cover.svelte';
  import { formatDuration } from './format';

  interface Props {
    albumId: number;
    onClose: () => void;
  }
  let { albumId, onClose }: Props = $props();

  let album = $state<AlbumDetail | null>(null);
  let error = $state('');

  $effect(() => {
    const id = albumId;
    album = null;
    error = '';
    api
      .album(id)
      .then((a) => {
        if (id === albumId) album = a;
      })
      .catch((e) => (error = e instanceof Error ? e.message : String(e)));
  });

  /** Group by disc, so a double album reads as two sides rather than one run. */
  let discs = $derived.by(() => {
    if (!album) return [];
    const map = new Map<number, typeof album.tracks>();
    for (const track of album.tracks) {
      const list = map.get(track.disc_no) ?? [];
      list.push(track);
      map.set(track.disc_no, list);
    }
    return [...map.entries()].sort((a, b) => a[0] - b[0]);
  });

  let nowPlayingId = $derived(player.track?.id);
</script>

<div class="detail">
  <button class="back" onclick={onClose}>← Library</button>

  {#if error}
    <p class="error">{error}</p>
  {:else if !album}
    <p class="dim">Loading…</p>
  {:else}
    <header>
      <div class="art">
        <Cover
          albumId={album.id}
          title={album.title}
          artist={album.artist}
          hasCover={album.has_cover}
        />
      </div>
      <div class="meta">
        <h1>{album.title}</h1>
        <p class="artist">{album.artist}</p>
        <p class="facts">
          {#if album.year}{album.year} · {/if}
          {album.track_count} tracks · {formatDuration(album.duration_ms)}
          {#if album.disc_count > 1} · {album.disc_count} discs{/if}
          {#if album.is_compilation} · compilation{/if}
        </p>
        <p class="facts">
          {#if album.gain_db != null}
            ReplayGain {album.gain_db > 0 ? '+' : ''}{album.gain_db.toFixed(1)} dB
          {:else}
            <span class="warn">no ReplayGain — volume may jump</span>
          {/if}
          {#if album.play_count > 0} · played {album.play_count}×{/if}
        </p>
        <div class="actions">
          <button class="primary" onclick={() => player.playAlbum(album!)}>Play album</button>
          <button onclick={() => player.enqueue(album!)}>Add to queue</button>
        </div>
      </div>
    </header>

    {#each discs as [discNo, tracks] (discNo)}
      {#if album.disc_count > 1}
        <h2 class="disc">Disc {discNo}</h2>
      {/if}
      <ol class="tracks">
        {#each tracks as track (track.id)}
          <li class:current={track.id === nowPlayingId}>
            <button onclick={() => player.playAlbum(album!, album!.tracks.indexOf(track))}>
              <span class="no">{track.track_no || '·'}</span>
              <span class="name">{track.title}</span>
              {#if track.artist !== album.artist}
                <span class="track-artist">{track.artist}</span>
              {/if}
              <span class="len">{formatDuration(track.duration_ms)}</span>
            </button>
          </li>
        {/each}
      </ol>
    {/each}
  {/if}
</div>

<style>
  .detail {
    padding: 1rem 1.25rem 8rem;
    max-width: 60rem;
  }
  .back {
    border: 0;
    background: none;
    color: var(--dim);
    cursor: pointer;
    padding: 0.3rem 0;
    font-size: 0.85rem;
  }
  .back:hover {
    color: var(--fg);
  }
  header {
    display: flex;
    gap: 1.4rem;
    margin: 0.8rem 0 1.6rem;
    flex-wrap: wrap;
  }
  .art {
    width: min(13rem, 40vw);
  }
  .meta {
    flex: 1 1 16rem;
  }
  h1 {
    margin: 0;
    font-size: 1.7rem;
    line-height: 1.15;
    letter-spacing: -0.02em;
  }
  .artist {
    margin: 0.3rem 0 0.6rem;
    color: var(--fg);
    opacity: 0.85;
  }
  .facts {
    margin: 0.2rem 0;
    font-size: 0.8rem;
    color: var(--dim);
  }
  .warn {
    color: var(--warn);
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 1rem;
    flex-wrap: wrap;
  }
  .actions button {
    padding: 0.5rem 1rem;
    border-radius: 999px;
    border: 1px solid var(--line);
    background: var(--raised);
    color: var(--fg);
    cursor: pointer;
    font-size: 0.85rem;
  }
  .actions .primary {
    background: var(--accent);
    color: #06121b;
    border-color: transparent;
    font-weight: 600;
  }
  .disc {
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--dim);
    margin: 1.4rem 0 0.4rem;
  }
  .tracks {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .tracks li button {
    display: grid;
    grid-template-columns: 2rem 1fr auto;
    gap: 0.8rem;
    align-items: baseline;
    width: 100%;
    padding: 0.45rem 0.6rem;
    border: 0;
    border-radius: 6px;
    background: none;
    color: inherit;
    text-align: left;
    cursor: pointer;
    font-size: 0.9rem;
  }
  .tracks li button:hover {
    background: var(--raised);
  }
  .tracks li.current button {
    color: var(--accent);
  }
  .no {
    color: var(--dim);
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  .name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .track-artist {
    grid-column: 2;
    font-size: 0.75rem;
    color: var(--dim);
  }
  .len {
    color: var(--dim);
    font-variant-numeric: tabular-nums;
    font-size: 0.8rem;
  }
  .error {
    color: var(--danger);
  }
  .dim {
    color: var(--dim);
  }
</style>
