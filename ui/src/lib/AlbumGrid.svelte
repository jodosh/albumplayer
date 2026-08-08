<script lang="ts">
  import type { AlbumSummary } from './api';
  import Cover from './Cover.svelte';

  interface Props {
    albums: AlbumSummary[];
    onOpen: (album: AlbumSummary) => void;
  }
  let { albums, onOpen }: Props = $props();
</script>

<div class="grid">
  {#each albums as album (album.id)}
    <button class="tile" onclick={() => onOpen(album)} title="{album.artist} — {album.title}">
      <Cover
        albumId={album.id}
        title={album.title}
        artist={album.artist}
        hasCover={album.has_cover}
      />
      <span class="title">{album.title}</span>
      <span class="artist">
        {album.artist}
        {#if album.year}<span class="year">· {album.year}</span>{/if}
      </span>
      {#if album.play_count > 0}
        <span class="plays" title="{album.play_count} full listens">▸{album.play_count}</span>
      {/if}
    </button>
  {/each}
</div>

<style>
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(9.5rem, 1fr));
    gap: 1.1rem 0.9rem;
    padding: 1rem 1.25rem 8rem;
  }
  .tile {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    padding: 0;
    border: 0;
    background: none;
    color: inherit;
    text-align: left;
    cursor: pointer;
    border-radius: 8px;
  }
  .tile:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 4px;
  }
  .tile :global(.cover) {
    transition: transform 0.12s ease;
  }
  .tile:hover :global(.cover) {
    transform: translateY(-2px);
  }
  .title {
    margin-top: 0.45rem;
    font-size: 0.82rem;
    line-height: 1.25;
    /* Two lines then ellipsis: album titles vary wildly in length and a ragged
       grid is harder to scan than a clipped one. */
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .artist {
    font-size: 0.75rem;
    color: var(--dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .year {
    opacity: 0.7;
  }
  .plays {
    position: absolute;
    top: 0.4rem;
    right: 0.4rem;
    background: rgb(0 0 0 / 0.65);
    color: #fff;
    font-size: 0.68rem;
    padding: 0.1rem 0.35rem;
    border-radius: 999px;
  }
</style>
