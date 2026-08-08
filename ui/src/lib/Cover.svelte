<script lang="ts">
  import { coverUrl } from './api';

  interface Props {
    albumId: number;
    title: string;
    artist: string;
    hasCover: boolean;
    size?: number;
  }
  let { albumId, title, artist, hasCover, size = 0 }: Props = $props();

  let failed = $state(false);

  /**
   * A quarter of this library has no cover anywhere, so the fallback has to be
   * a real design rather than a grey square: a stable colour derived from the
   * album, with its initials.
   */
  function hue(text: string): number {
    let h = 0;
    for (let i = 0; i < text.length; i++) h = (h * 31 + text.charCodeAt(i)) % 360;
    return h;
  }

  let initials = $derived(
    title
      .replace(/^(the|a|an)\s+/i, '')
      .split(/\s+/)
      .filter((w) => /[a-z0-9]/i.test(w))
      .slice(0, 2)
      .map((w) => w[0].toUpperCase())
      .join('') || '♪',
  );
  let tint = $derived(hue(`${artist}${title}`));
  let showImage = $derived(hasCover && !failed);
</script>

<div
  class="cover"
  style:--tint={tint}
  style:width={size ? `${size}px` : null}
  style:height={size ? `${size}px` : null}
>
  {#if showImage}
    <img src={coverUrl(albumId)} alt="" loading="lazy" onerror={() => (failed = true)} />
  {:else}
    <span class="initials" aria-hidden="true">{initials}</span>
  {/if}
</div>

<style>
  .cover {
    position: relative;
    aspect-ratio: 1;
    width: 100%;
    border-radius: 6px;
    overflow: hidden;
    background: linear-gradient(
      145deg,
      hsl(var(--tint) 38% 32%),
      hsl(calc(var(--tint) + 40) 42% 18%)
    );
    display: grid;
    place-items: center;
    flex: none;
  }
  img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    display: block;
  }
  .initials {
    font-size: clamp(1rem, 30cqw, 3rem);
    font-weight: 600;
    letter-spacing: 0.05em;
    color: hsl(var(--tint) 30% 88% / 0.9);
    container-type: inline-size;
  }
</style>
