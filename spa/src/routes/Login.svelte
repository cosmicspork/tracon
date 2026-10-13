<script lang="ts">
  import { isTauri } from '@tauri-apps/api/core'

  import DeviceCode from '../components/DeviceCode.svelte'
  import { api, ApiError } from '../lib/api'
  import { insecureContext, takeToken } from '../lib/auth'
  import {
    DeviceCancelled,
    deviceProblem,
    pollForApproval,
    stashedApproval,
    type DeviceStart,
  } from '../lib/device'
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

  // Signing in by approval: a browser already signed in to this node (with a
  // passkey) approves this one. The desktop app's way in to a remote node,
  // and any browser's that has no token to hand.
  const desktop = typeof window !== 'undefined' && isTauri()
  const approving = stashedApproval() !== null
  let device = $state<DeviceStart | null>(null)
  let deviceError = $state<string | null>(null)
  let asking = $state(false)
  let abort: AbortController | null = null

  async function signInElsewhere() {
    if (asking || device) return
    asking = true
    deviceError = null
    abort = new AbortController()
    const signal = abort.signal
    try {
      const started = await api.deviceSignIn()
      device = started
      await pollForApproval(started, (code) => api.deviceToken(code), { signal })
      device = null
      store.signedIn()
    } catch (err) {
      device = null
      if (!(err instanceof DeviceCancelled)) deviceError = deviceProblem(err)
    } finally {
      asking = false
      abort = null
    }
  }

  function stopAsking() {
    abort?.abort()
  }

  // Signed in some other way, or gone: stop polling.
  $effect(() => () => abort?.abort())

  async function submit(e: SubmitEvent) {
    e.preventDefault()
    if (!token.trim() || busy) return
    await signIn(token.trim())
  }
</script>

<main>
  <form onsubmit={submit}>
    <h1>tracon</h1>
    {#if approving}
      <p class="note">Sign in to see the request you were sent here to approve.</p>
    {/if}
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
    {#if !approving}
      <div class="or">
        {#if device}
          <DeviceCode start={device} {desktop} />
          <button class="lnk" type="button" onclick={stopAsking}>Cancel</button>
        {:else}
          <p>
            {desktop
              ? 'Or approve this window from a browser already signed in to the node, with its passkey.'
              : 'Or approve this browser from another one already signed in to the node, with its passkey.'}
          </p>
          <button class="alt" type="button" onclick={() => void signInElsewhere()} disabled={asking} data-testid="device-sign-in">
            {asking ? 'Asking…' : 'Sign in from another browser'}
          </button>
        {/if}
        {#if deviceError}<p class="err" data-testid="device-sign-in-problem">{deviceError}</p>{/if}
      </div>
    {/if}
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
  .note {
    color: var(--ink);
  }
  .or {
    display: grid;
    gap: 10px;
    border-top: 1px solid var(--s2);
    padding-top: 14px;
  }
  button.lnk {
    justify-self: start;
    background: none;
    color: var(--ink2);
    padding: 0;
    font: 13px var(--sans);
  }
  button.alt {
    background: var(--s2);
    color: var(--ink);
  }
  small {
    color: var(--dim);
    font-size: 12px;
    line-height: 1.45;
  }
</style>
