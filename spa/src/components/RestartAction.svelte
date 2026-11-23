<script lang="ts">
  // The one "Restart" control, shared by the banner and Settings › System ›
  // Restart. A click reads the node's restart status again and decides on
  // it: with nothing running and no session live it restarts at once;
  // otherwise one inline step names what is running and offers the ways on,
  // each one more click. A restart that is waiting can be withdrawn, or
  // hurried along. Restarting is an administrator's action: off the node the
  // passkey prompt (`StepUpPrompt`) confirms it. `node` restarts a peer
  // administered from here instead, whose status the caller holds (`peer`)
  // and is handed back fresh (`onstatus`) after each step.
  import { api } from '../lib/api'
  import { peerFailure } from '../lib/peer-admin'
  import { list, restartAsk, type RestartAsk } from '../lib/restart'
  import { store } from '../lib/store.svelte'
  import type { RestartStatus } from '../lib/types'

  let {
    variant = 'banner',
    node = '',
    peer = null,
    onstatus,
  }: {
    variant?: 'banner' | 'card'
    node?: string
    peer?: RestartStatus | null
    onstatus?: (status: RestartStatus) => void
  } = $props()

  /** Read the restart status again, of this node or of the peer. */
  async function refresh(): Promise<RestartStatus | null> {
    if (!node) {
      await store.refreshRestart()
      return store.restart
    }
    const fresh = await api.restartStatus(node)
    onstatus?.(fresh)
    return fresh
  }

  type Confirm = Extract<RestartAsk, { kind: 'confirm' }>

  let confirming = $state<Confirm | null>(null)
  let busy = $state(false)
  let error = $state('')

  const status = $derived(node ? peer : store.restart)
  const plan = $derived(status?.plan ?? null)
  const unavailable = $derived(status ? restartAsk(status).kind === 'unavailable' : true)
  const cls = $derived(variant === 'card' ? 'btn' : 'lnk')

  async function act(run: () => Promise<unknown>) {
    busy = true
    error = ''
    try {
      await run()
      confirming = null
    } catch (e) {
      error = peerFailure(store.nodes, node, e)
    } finally {
      busy = false
      // A refusal (the file changed since it was read, a restart already
      // waiting) is shown against the status as it is now.
      await refresh().catch(() => null)
    }
  }

  async function restart() {
    if (busy) return
    busy = true
    error = ''
    confirming = null
    let ask: RestartAsk
    let hash: string | undefined
    try {
      // Decided against what runs now, not what the page last heard.
      const fresh = await api.restartStatus(node)
      if (node) onstatus?.(fresh)
      else store.restart = fresh
      ask = restartAsk(fresh)
      hash = fresh.config?.hash
    } catch (e) {
      error = `could not read the node's restart status: ${peerFailure(store.nodes, node, e)}`
      busy = false
      return
    }
    busy = false
    if (ask.kind === 'unavailable') {
      error = ask.reason
      return
    }
    if (ask.kind === 'confirm') {
      confirming = ask
      return
    }
    // `when_idle` even with nothing running: a turn that starts meanwhile is
    // waited for rather than cut off.
    if (hash) await act(() => api.restart('when_idle', hash, false, node))
  }

  function send(mode: 'when_idle' | 'now') {
    const hash = status?.config?.hash
    if (!hash) return
    return act(() => api.restart(mode, hash, mode === 'now', node))
  }

  function endLabel(c: Confirm): string {
    if (c.canWait) return 'End them and restart now'
    return `End ${c.sessions.length} and restart`
  }
</script>

{#if plan?.state === 'draining'}
  <button class={cls} type="button" disabled={busy} onclick={() => act(() => api.cancelRestart(node))}>Don't restart</button>
  <button class="{cls} d" type="button" disabled={busy} onclick={() => send('now')}
    >{status?.closing.length ? 'End them and restart now' : 'Restart now'}</button
  >
{:else if !plan}
  <button class={cls} type="button" disabled={busy || unavailable} onclick={restart}>Restart</button>
  {#if confirming}
    {@const c = confirming}
    <i class="confirm">
      {#if c.running.length}
        <span>Running now: {list(c.running)}.</span>
      {/if}
      {#if c.sessions.length}
        <span
          >Restarting ends {c.sessions.length} live session{c.sessions.length === 1 ? '' : 's'}{c.sessions.some((s) => s.turn_active)
            ? ', cutting off turns that are running'
            : ''}{c.canWait ? ' unless you wait for them' : ''}:</span
        >
        {#each c.sessions as s (s.id)}<span class="session">{s.branch || s.id} on {s.channel}{s.turn_active ? ' (working)' : ''}</span>{/each}
      {/if}
      {#if c.canWait}
        <button class={cls} type="button" disabled={busy} onclick={() => send('when_idle')} title="Waits up to 30 minutes; new sessions and turns wait until the node is back"
          >Restart when they finish</button
        >
      {/if}
      <button class="{cls} d" type="button" disabled={busy} onclick={() => send('now')}>{endLabel(c)}</button>
      <button class="lnk" type="button" disabled={busy} onclick={() => (confirming = null)}>Cancel</button>
    </i>
  {/if}
{/if}
{#if error}<i class="err" role="alert">{error}</i>{/if}

<style>
  .confirm {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px 10px;
  }
  .confirm span {
    flex-basis: 100%;
  }
  .session {
    display: block;
    overflow-wrap: anywhere;
  }
  .err {
    color: var(--crit);
  }
</style>
