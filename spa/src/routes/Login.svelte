<script lang="ts">
  import { ApiError } from '../lib/api'
  import { insecureContext, takeToken } from '../lib/auth'
  import { store } from '../lib/store.svelte'

  let token = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)

  // The cookie is Secure: over plain http off this machine the exchange would
  // succeed and the cookie silently vanish, looping back here. Say so instead.
  const insecure = insecureContext(location.protocol, location.hostname)

  async function signIn(t: string) {
    busy = true
    error = null
    try {
      await store.signIn(t)
      token = ''
    } catch (err) {
      error =
        err instanceof ApiError
          ? err.message
          : 'the node did not answer; check that it is still running'
    } finally {
      busy = false
    }
  }

  // A scanned login QR left its token in the stash; spend it. Runs once —
  // the stash empties on take, so a failure falls back to the form.
  $effect(() => {
    const t = takeToken()
    if (t && !insecure) void signIn(t)
  })

  async function submit(e: SubmitEvent) {
    e.preventDefault()
    if (!token.trim() || busy) return
    await signIn(token.trim())
  }
</script>

<main>
  <form onsubmit={submit}>
    <h1>tracon</h1>
    <p>
      This node is reached from off its own machine, so it wants the operator token. Run
      <code>tracon auth issue</code> on the node to mint one.
    </p>
    <!-- svelte-ignore a11y_autofocus -->
    <input
      type="password"
      bind:value={token}
      placeholder="trc1.…"
      autocomplete="current-password"
      autocapitalize="off"
      autocorrect="off"
      spellcheck="false"
      autofocus
      disabled={busy}
    />
    {#if insecure}
      <p class="err">
        This page is plain http, so the login cookie cannot be kept. Reach the
        node over HTTPS — an ingress, a reverse proxy, or a tailnet name.
      </p>
    {/if}
    {#if error}<p class="err">{error}</p>{/if}
    <button type="submit" disabled={busy || !token.trim()}>{busy ? 'Checking…' : 'Log in'}</button>
    <small>The token is exchanged for a cookie this browser holds. It is not stored here.</small>
  </form>
</main>

<style>
  main {
    min-height: 100dvh;
    display: grid;
    place-items: center;
    padding: 24px;
  }
  form {
    display: grid;
    gap: 12px;
    width: min(360px, 100%);
    background: var(--s1);
    border-radius: 6px;
    padding: 28px 24px;
  }
  h1 {
    margin: 0;
    font: 500 20px var(--sans);
    letter-spacing: 0.02em;
  }
  p {
    margin: 0;
    color: var(--ink2);
    font-size: 13.5px;
    line-height: 1.5;
  }
  code {
    font: 12.5px var(--mono);
    color: var(--ink);
  }
  input {
    background: var(--s2);
    border: none;
    border-radius: 4px;
    padding: 10px 12px;
    color: var(--ink);
    font: 14px var(--mono);
  }
  input:focus-visible {
    outline: 2px solid var(--acc);
    outline-offset: 1px;
  }
  button {
    background: var(--acc);
    color: var(--acc-ink);
    border: none;
    border-radius: 4px;
    padding: 10px 14px;
    font: 500 14px var(--sans);
    cursor: pointer;
  }
  button:disabled {
    opacity: 0.55;
    cursor: default;
  }
  .err {
    color: var(--crit);
    font: 12.5px var(--mono);
  }
  small {
    color: var(--dim);
    font-size: 12px;
    line-height: 1.45;
  }
</style>
