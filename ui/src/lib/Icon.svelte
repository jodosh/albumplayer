<script lang="ts">
  /**
   * Inline SVG rather than emoji glyphs. Emoji render differently on every
   * platform, arrive in colour whether you want it or not, and cannot inherit
   * the current text colour — all of which matters in a transport bar.
   */
  type Name =
    | 'prev-album'
    | 'prev'
    | 'play'
    | 'pause'
    | 'next'
    | 'next-album'
    | 'shuffle'
    | 'repeat'
    | 'repeat-one';

  let { name, size = 18 }: { name: Name; size?: number } = $props();
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.9"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {#if name === 'play'}
    <path d="M7 4.5 19 12 7 19.5Z" fill="currentColor" stroke-width="1" />
  {:else if name === 'pause'}
    <path d="M9 5v14M15 5v14" />
  {:else if name === 'prev'}
    <path d="M17 5.5 8 12l9 6.5Z" fill="currentColor" stroke-width="1" />
    <path d="M6 5v14" />
  {:else if name === 'next'}
    <path d="M7 5.5 16 12l-9 6.5Z" fill="currentColor" stroke-width="1" />
    <path d="M18 5v14" />
  {:else if name === 'prev-album'}
    <path d="M20 6.5 13 12l7 5.5Z" fill="currentColor" stroke-width="1" />
    <path d="M11 6.5 4 12l7 5.5Z" fill="currentColor" stroke-width="1" />
    <path d="M2.5 6v12" />
  {:else if name === 'next-album'}
    <path d="M4 6.5 11 12l-7 5.5Z" fill="currentColor" stroke-width="1" />
    <path d="M13 6.5 20 12l-7 5.5Z" fill="currentColor" stroke-width="1" />
    <path d="M21.5 6v12" />
  {:else if name === 'shuffle'}
    <path d="M16 4h4v4M20 4l-6.5 6.5M16 20h4v-4M20 20l-7-7M4 4l4.5 4.5M4 20l6.5-6.5" />
  {:else if name === 'repeat'}
    <path d="M4 12V9a3 3 0 0 1 3-3h12M16 3l3 3-3 3M20 12v3a3 3 0 0 1-3 3H5M8 21l-3-3 3-3" />
  {:else if name === 'repeat-one'}
    <path d="M4 12V9a3 3 0 0 1 3-3h12M16 3l3 3-3 3M20 12v3a3 3 0 0 1-3 3H5M8 21l-3-3 3-3" />
    <path d="M12 10.5v4M12 10.5l-1 .8" stroke-width="1.7" />
  {/if}
</svg>

<style>
  svg {
    display: block;
  }
</style>
