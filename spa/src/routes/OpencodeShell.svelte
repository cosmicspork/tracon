<script lang="ts">
  // OpenCode's own interface, inside the installed app.
  //
  // The view is a cross-origin `<iframe>` and that is the point: the native UI
  // runs on the node's dedicated origin (`node/src/http/ui.rs`), holds a
  // capability for one session, and can reach neither this page's cookie nor
  // this page's DOM. What this route adds is the *frame* — a route inside the
  // PWA's scope, so an installed app never hands the session to the system
  // browser, and a bar slim enough to leave the native view the screen.
  //
  // Three decisions worth stating, because each one looks like an omission:
  //
  //  * **No `sandbox`.** A sandboxed frame has an opaque origin, and the UI
  //    origin refuses `Origin: null` outright — a sandbox here would not
  //    tighten the boundary, it would break the boot exchange. The isolation
  //    is the separate origin, enforced on the node.
  //  * **No `postMessage` bridge.** The child is given no capability and asked
  //    for nothing. Everything this bar shows comes from tracon's own session
  //    API, which this page is already authorised for.
  //  * **The boot URL is only ever an `src`.** It carries a single-use
  //    capability in its fragment; it is never pushed to history, never
  //    logged, and every message about it goes through `redact`.

  import { api } from '../lib/api'
  import { humanizeError } from '../lib/errors'
  import { frameSrc, mint, redact, staleAfter } from '../lib/opencode'
  import { store } from '../lib/store.svelte'
  import { isTerminal, type Session } from '../lib/types'

  let { id }: { id: string } = $props()

  let src = $state<string | null>(null)
  let failure = $state<string | null>(null)
  /** Set only when the embedded view is *certainly* finished: the session
      ended, or the capability it booted on has outlived the node's cookie.
      Never a guess — a live frame must not be replaced by a reconnect button. */
  let finished = $state<string | null>(null)
  let booting = $state(false)
  /** Re-creating the element is what reconnecting means, so the frame is keyed
      on this rather than having its `src` reassigned. */
  let generation = $state(0)
  let bootedMs = 0
  let cookieTtlMs: number | undefined
  /** The session as the node last described it, for the moments before the
      event stream has caught up on a cold open. */
  let loaded = $state<Session | null>(null)

  const session = $derived(store.sessions.get(id) ?? loaded ?? undefined)
  const title = $derived(session?.branch ?? id.slice(0, 8))
  const ended = $derived(session !== undefined && isTerminal(session.state))
  const chip = $derived(session ? session.state.replace(/_/g, ' ') : 'loading')

  async function connect() {
    booting = true
    failure = null
    finished = null
    try {
      const minted = await mint(id)
      // Validate before framing: a node whose boot URL drifted from its own
      // configured origin, or put the capability where a log would keep it,
      // is refused here as well as in the desktop wrapper.
      const next = frameSrc(minted, location.origin)
      bootedMs = Date.now()
      cookieTtlMs = minted.cookie_ttl_ms
      src = next
      generation += 1
    } catch (err) {
      // `humanizeError` keeps the node's own sentence; nothing here appends a
      // URL, because the only URL in hand carries a capability.
      failure = humanizeError(err instanceof Error ? err.message : String(err))
    } finally {
      booting = false
    }
  }

  /**
   * After a suspension — an installed app on a phone is backgrounded far more
   * often than it is closed — ask the node what is true before trusting what
   * is on screen. A frame whose session ended, or whose cookie the node has
   * long since expired, shows nothing useful and says nothing about it.
   */
  async function recheck() {
    if (typeof document !== 'undefined' && document.visibilityState !== 'visible') return
    if (!src) return
    try {
      const result = await api.session(id)
      loaded = result.session
      if (isTerminal(result.session.state)) {
        finished = `This session ${result.session.state.replace(/_/g, ' ')} while the app was away.`
        return
      }
    } catch (err) {
      finished = humanizeError(err instanceof Error ? err.message : String(err))
      return
    }
    if (Date.now() >= staleAfter(bootedMs, cookieTtlMs)) {
      finished = 'The capability this view was opened with has expired.'
    }
  }

  $effect(() => {
    void id
    src = null
    failure = null
    finished = null
    loaded = null
    api
      .session(id)
      .then((result) => (loaded = result.session))
      .catch(() => {})
    void connect()
  })

  $effect(() => {
    const resume = () => void recheck()
    document.addEventListener('visibilitychange', resume)
    window.addEventListener('pageshow', resume)
    return () => {
      document.removeEventListener('visibilitychange', resume)
      window.removeEventListener('pageshow', resume)
    }
  })
</script>

<div class="oc">
  <header>
    <a class="back" href="/sessions/{id}" aria-label="Back to the session">‹ Back</a>
    <span class="ttl" title={src ? `OpenCode at ${redact(src)}` : 'OpenCode'}>{title}</span>
    <span class="chip" class:warn={ended}>{chip}</span>
  </header>

  {#if failure}
    <div class="msg">
      <p class="crit">{failure}</p>
      <button type="button" onclick={() => void connect()} disabled={booting}>Try again</button>
    </div>
  {:else if finished}
    <div class="msg">
      <p>{finished}</p>
      {#if ended}
        <a href="/sessions/{id}">Back to the session</a>
      {:else}
        <button type="button" onclick={() => void connect()} disabled={booting}>Reconnect</button>
      {/if}
    </div>
  {:else if src}
    {#key generation}
      <iframe
        class="view"
        title="OpenCode — {title}"
        {src}
        referrerpolicy="no-referrer"
        allow=""
      ></iframe>
    {/key}
  {:else}
    <div class="msg"><p>Opening OpenCode…</p></div>
  {/if}
</div>

<style>
  /* The insets are on the frame rather than the page: the native view is the
     content, and a notch must not eat it. `dvh` so an address bar sliding
     away does not leave the iframe taller than the screen. */
  .oc {
    height: 100dvh;
    box-sizing: border-box;
    display: grid;
    grid-template-rows: auto 1fr;
    overflow: hidden;
    background: var(--bg);
    padding-top: env(safe-area-inset-top);
    padding-bottom: env(safe-area-inset-bottom);
    padding-left: env(safe-area-inset-left);
    padding-right: env(safe-area-inset-right);
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    min-width: 0;
    padding: 0.45rem 0.7rem;
    border-bottom: 1px solid var(--rule);
    background: var(--s1);
  }
  .back {
    flex: none;
    font: 12px var(--mono);
    text-decoration: none;
  }
  .ttl {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font: 12px var(--mono);
    color: var(--ink2);
  }
  .chip {
    flex: none;
    font: 11px var(--mono);
    color: var(--ink2);
    border: 1px solid var(--rule);
    border-radius: 999px;
    padding: 0.1rem 0.5rem;
    white-space: nowrap;
  }
  .chip.warn {
    color: var(--crit);
    border-color: var(--crit);
  }
  .view {
    width: 100%;
    height: 100%;
    min-width: 0;
    min-height: 0;
    border: 0;
    display: block;
    background: var(--bg);
  }
  .msg {
    padding: 1.5rem 1rem;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0.8rem;
    min-width: 0;
  }
  .msg p {
    margin: 0;
    max-width: 100%;
    overflow-wrap: anywhere;
  }
  .crit {
    color: var(--crit);
  }
</style>
