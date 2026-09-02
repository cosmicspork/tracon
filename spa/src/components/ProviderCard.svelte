<script lang="ts">
  // One provider on one node — this node's or a peer's. Actions go through
  // the node-scoped endpoints: the serving node runs them itself or seals the
  // command to the owner, and the login subprocess never leaves the owner.
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatDuration } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import type { ProviderInfo } from '../lib/types'

  let { p, nodeId }: { p: ProviderInfo; nodeId: string } = $props()

  let code = $state('')
  let busy = $state(false)
  let error = $state('')
  // A peer's mirrored state lags its ack by a heartbeat; the URL from a
  // successful connect is shown at once and dropped when the mirror catches up.
  let justUrl = $state<string | null>(null)
  $effect(() => {
    if (p.state !== 'disconnected') justUrl = null
  })
  const shownState = $derived(justUrl && p.state === 'disconnected' ? 'pending' : p.state)
  const shownUrl = $derived(p.url ?? justUrl)

  async function act(f: () => Promise<unknown>) {
    busy = true
    error = ''
    try {
      await f()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
  function connect() {
    return act(async () => {
      const d = await api.nodeConnectProvider(
        nodeId,
        p.name,
        store.channels.filter((c) => !c.archived).map((c) => c.name),
      )
      justUrl = d.url
    })
  }
  function paste() {
    const c = code.trim()
    if (!c) return
    return act(async () => {
      await api.nodeProviderCode(nodeId, p.name, c)
      code = ''
    })
  }
  function disconnect() {
    return act(async () => {
      await api.nodeDisconnectProvider(nodeId, p.name)
      justUrl = null
    })
  }
  function expiry(): string {
    if (!p.expires_ms) return ''
    const left = p.expires_ms - clock.now
    return left <= 0 ? 'expired' : `refreshes in ${formatDuration(left)}`
  }
</script>

<div class="prov" class:pending={shownState === 'pending'} class:bad={p.state === 'failed'}>
  <span class="pbar"></span>
  <span class="pnm">
    {p.name}
    <small
      >{#if p.state === 'connected'}{p.kind === 'oauth' ? 'subscription' : 'api key'}{#if p.identity}
          · {p.identity}{/if}{:else if shownState === 'pending'}waiting on you{:else if p.state === 'failed'}failed{:else}not
        connected{/if}</small
    >
  </span>
  <span class="pst">
    {#if p.state === 'connected'}
      <span class="l"><span class="chip">connected</span>{#if p.channels.length} · {p.channels.join(', ')}{/if}{#if expiry()} · {expiry()}{/if}</span>
      <span><button class="lnk d" onclick={disconnect} disabled={busy}>Disconnect</button></span>
    {:else if shownState === 'pending'}
      <span class="l warn"><span class="chip warn">connect</span> · open the link, sign in, paste what it gives you back</span>
      {#if shownUrl}
        <span><a class="lnk" href={shownUrl} target="_blank" rel="noopener">Open {p.name} sign-in</a></span>
      {/if}
      <span class="paste">
        <input
          placeholder="redirect URL or code"
          bind:value={code}
          onkeydown={(e) => e.key === 'Enter' && paste()}
        />
        <button class="btn p" onclick={paste} disabled={busy || !code.trim()}>Paste back</button>
        <button class="lnk d" onclick={disconnect} disabled={busy}>Cancel</button>
      </span>
    {:else}
      <span class="l off" class:bad={p.state === 'failed'}
        ><span class="chip" class:off={p.state !== 'failed'} class:bad={p.state === 'failed'}>{p.state === 'failed' ? 'failed' : 'disconnected'}</span>{#if p.error} · {p.error}{/if}</span
      >
      {#if p.can_login}
        <span><button class="lnk" onclick={connect} disabled={busy}>{p.state === 'failed' ? 'Try again' : 'Connect'}</button></span>
      {:else}
        <span>API key only: <code>tracon credential import</code> on its node.</span>
      {/if}
    {/if}
    {#if error}<span class="l bad">{error}</span>{/if}
  </span>
</div>

<style>
  .prov {
    display: grid;
    grid-template-columns: 3px 133px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    padding: 9px 14px 9px 0;
    overflow: hidden;
  }
  .pbar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--ok);
  }
  .prov.pending .pbar {
    background: var(--wait);
  }
  .prov.pending {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  .prov.bad .pbar {
    background: var(--crit);
  }
  .pnm {
    font-weight: 600;
    min-width: 0;
  }
  .pnm small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    font-weight: 400;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pst {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    min-width: 0;
  }
  .pst > span {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pst .l.warn {
    color: var(--wait);
  }
  .pst .l.off {
    color: var(--dim);
  }
  .pst .l.bad {
    color: var(--crit);
  }
  .paste {
    display: flex;
    gap: 8px;
    align-items: center;
    white-space: normal;
  }
  .paste input {
    flex: 1;
    min-width: 0;
    font: 12.5px var(--mono);
    background: var(--s2);
    color: var(--ink);
    border: 0;
    border-radius: 3px;
    padding: 6px 8px;
  }
  .pst code {
    font: 12px var(--mono);
    color: var(--ink);
  }
  @media (max-width: 700px) {
    /* Signing in to a provider is a phone job — the sign-in happens where the
       password manager is. Stack the paste-back; 16px stops iOS zoom. */
    .prov {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 4px 12px;
    }
    .pst {
      grid-column: 2;
    }
    .paste {
      flex-wrap: wrap;
    }
    .paste input {
      flex-basis: 100%;
      font-size: 16px;
    }
  }
</style>
