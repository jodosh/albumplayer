<script lang="ts">
  import * as api from './lib/api';
  import type { AlbumSummary } from './lib/api';
  import { player, nativePlayback } from './lib/player.svelte';
  import Login from './lib/Login.svelte';
  import AlbumGrid from './lib/AlbumGrid.svelte';
  import AlbumDetail from './lib/AlbumDetail.svelte';
  import NowPlaying from './lib/NowPlaying.svelte';

  let signedIn = $state(api.token() !== null);
  let albums = $state<AlbumSummary[]>([]);
  let loading = $state(false);
  let error = $state('');
  let search = $state('');
  let sort = $state('artist');
  let openAlbumId = $state<number | null>(null);

  let primary: HTMLAudioElement;
  let secondary: HTMLAudioElement;

  $effect(() => {
    // In the desktop shell the Rust engine plays the audio and `attach` merely
    // subscribes to its events; in a browser it wires up the two elements.
    if (nativePlayback) player.attach();
    else if (primary && secondary) player.attach(primary, secondary);
  });

  $effect(() => {
    if (!signedIn) return;
    // Re-runs when either control changes.
    const params = { sort, search: search.trim() || undefined };
    loading = true;
    error = '';
    api
      .albums(params)
      .then((rows) => (albums = rows))
      .catch((e) => {
        if (e instanceof api.Unauthorized) signedIn = false;
        else error = e instanceof Error ? e.message : String(e);
      })
      .finally(() => (loading = false));
  });

  // Settle the listening session rather than dropping it on the floor.
  $effect(() => {
    const bye = () => void player.shutdown();
    window.addEventListener('pagehide', bye);
    return () => window.removeEventListener('pagehide', bye);
  });

  function signOut() {
    api.clearToken();
    signedIn = false;
    albums = [];
    openAlbumId = null;
  }

  async function shuffleEverything() {
    // The grid only carries summaries; the player needs full tracklists.
    const detailed = await Promise.all(albums.slice(0, 400).map((a) => api.album(a.id)));
    await player.playAll(detailed, true);
  }
</script>

{#if !signedIn}
  <Login onDone={() => (signedIn = true)} />
{:else}
  <header class="top">
    <button class="brand" onclick={() => (openAlbumId = null)}>AlbumPlayer</button>

    <input
      class="search"
      type="search"
      placeholder="Search albums and artists…"
      bind:value={search}
      oninput={() => (openAlbumId = null)}
    />

    <select bind:value={sort} aria-label="Sort albums">
      <option value="artist">Artist</option>
      <option value="title">Title</option>
      <option value="year">Newest</option>
      <option value="plays">Most played</option>
      <option value="added">Recently added</option>
      <option value="last">Last played</option>
    </select>

    <button onclick={shuffleEverything} title="Shuffle albums — track order is kept">
      Shuffle albums
    </button>
    <button class="quiet" onclick={signOut}>Sign out</button>
  </header>

  <main>
    {#if error}
      <p class="error">{error}</p>
    {:else if openAlbumId !== null}
      <AlbumDetail albumId={openAlbumId} onClose={() => (openAlbumId = null)} />
    {:else if loading && albums.length === 0}
      <p class="dim">Loading library…</p>
    {:else if albums.length === 0}
      <p class="dim">No albums matched.</p>
    {:else}
      <p class="count">{albums.length} albums</p>
      <AlbumGrid {albums} onOpen={(a) => (openAlbumId = a.id)} />
    {/if}
  </main>

  <NowPlaying />
{/if}

{#if !nativePlayback}
  <!-- Two elements so the next track is buffered before the current one ends.
       The desktop shell has no need for them: GStreamer handles the handover. -->
  <audio bind:this={primary} preload="auto"></audio>
  <audio bind:this={secondary} preload="auto"></audio>
{/if}

<style>
  .top {
    position: sticky;
    top: 0;
    z-index: 5;
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.7rem 1.25rem;
    background: var(--bar);
    border-bottom: 1px solid var(--line);
    backdrop-filter: blur(12px);
  }
  .brand {
    font-weight: 600;
    letter-spacing: -0.01em;
    border: 0;
    background: none;
    color: var(--fg);
    cursor: pointer;
    font-size: 0.95rem;
    padding: 0;
  }
  .search {
    flex: 1 1 auto;
    min-width: 6rem;
    max-width: 26rem;
    padding: 0.45rem 0.7rem;
    border-radius: 999px;
    border: 1px solid var(--line);
    background: var(--raised);
    color: var(--fg);
    font-size: 0.85rem;
  }
  select,
  .top button:not(.brand) {
    padding: 0.45rem 0.7rem;
    border-radius: 6px;
    border: 1px solid var(--line);
    background: var(--raised);
    color: var(--fg);
    font-size: 0.8rem;
    cursor: pointer;
  }
  .top .quiet {
    border-color: transparent;
    background: none;
    color: var(--dim);
  }
  .count {
    margin: 0;
    padding: 0.9rem 1.25rem 0;
    font-size: 0.75rem;
    color: var(--dim);
  }
  .error {
    padding: 1.25rem;
    color: var(--danger);
  }
  .dim {
    padding: 1.25rem;
    color: var(--dim);
  }
  audio {
    display: none;
  }

  @media (max-width: 40rem) {
    select,
    .top .quiet {
      display: none;
    }
  }
</style>
