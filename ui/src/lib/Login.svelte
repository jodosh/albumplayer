<script lang="ts">
  import { login, setBaseUrl, needsServerAddress } from './api';

  let { onDone }: { onDone: () => void } = $props();

  let password = $state('');
  let server = $state(localStorage.getItem('albumplayer.base') ?? '');
  let error = $state('');
  let busy = $state(false);

  // The desktop shell has no origin to fall back on, so the address is required
  // there rather than optional.
  const serverRequired = needsServerAddress();

  async function submit(event: Event) {
    event.preventDefault();
    busy = true;
    error = '';
    try {
      // The desktop shell points at a homelab address; served from the server
      // itself, this stays blank and the current origin is used.
      if (server.trim()) setBaseUrl(server.trim());
      else if (serverRequired) throw new Error('Enter the address of your server');
      await login(password);
      password = '';
      onDone();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="wrap">
  <form onsubmit={submit}>
    <h1>AlbumPlayer</h1>
    <p class="tagline">Albums, not shuffle.</p>

    <label>
      Server {#if serverRequired}<span class="req">required</span>{/if}
      <input
        type="url"
        bind:value={server}
        placeholder={serverRequired ? 'http://homelab.lan:8080' : window.location.origin}
        required={serverRequired}
        autocomplete="url"
      />
    </label>

    <label>
      Password
      <!-- svelte-ignore a11y_autofocus -->
      <input type="password" bind:value={password} autocomplete="current-password" autofocus />
    </label>

    {#if error}<p class="error" role="alert">{error}</p>{/if}

    <button type="submit" disabled={busy || !password || (serverRequired && !server.trim())}>
      {busy ? 'Signing in…' : 'Sign in'}
    </button>
  </form>
</div>

<style>
  .wrap {
    min-height: 100dvh;
    display: grid;
    place-items: center;
    padding: 1.5rem;
  }
  form {
    width: min(22rem, 100%);
    display: flex;
    flex-direction: column;
    gap: 0.9rem;
  }
  h1 {
    margin: 0;
    font-size: 1.6rem;
    letter-spacing: -0.02em;
  }
  .tagline {
    margin: -0.6rem 0 0.6rem;
    color: var(--dim);
    font-size: 0.9rem;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    font-size: 0.8rem;
    color: var(--dim);
  }
  input {
    padding: 0.6rem 0.7rem;
    border-radius: 6px;
    border: 1px solid var(--line);
    background: var(--raised);
    color: var(--fg);
    font-size: 0.95rem;
  }
  input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .error {
    margin: 0;
    color: var(--danger);
    font-size: 0.85rem;
  }
  .req {
    color: var(--warn);
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  button {
    margin-top: 0.3rem;
    padding: 0.6rem;
    border: 0;
    border-radius: 6px;
    background: var(--accent);
    color: #06121b;
    font-weight: 600;
    cursor: pointer;
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
