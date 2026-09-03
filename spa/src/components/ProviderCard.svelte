<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import {
    browserCanClaimNode,
    desktopManagedLocal,
    openExternal,
    prepareExternalOpen,
  } from '../lib/external'
  import { formatDuration } from '../lib/format'
  import { completionInstruction, providerLabel } from '../lib/providers'
  import { store } from '../lib/store.svelte'
  import type { ProviderConnectResult, ProviderInfo } from '../lib/types'

  let { p, nodeId }: { p: ProviderInfo; nodeId: string } = $props()

  let code = $state('')
  let busy = $state(false)
  let error = $state('')
  let justResult = $state<ProviderConnectResult | null>(null)
  let managedLocal = $state(false)
  let sameHostBrowser = $state(false)

  const isSelf = $derived(nodeId === store.node?.id)
  const mayClaimBrowser = $derived(isSelf && !managedLocal && browserCanClaimNode())
  const shownState = $derived(justResult && p.state === 'disconnected' ? 'pending' : p.state)
  const shownUrl = $derived(p.url ?? justResult?.url ?? null)
  const completion = $derived(p.completion ?? justResult?.completion ?? null)
  const completionNote = $derived(p.completion_note ?? justResult?.completion_note ?? null)
  const deviceCode = $derived(p.device_code ?? justResult?.device_code ?? null)

  $effect(() => {
    if (p.state === 'connected' || p.state === 'failed') justResult = null
  })

  onMount(() => {
    void desktopManagedLocal().then((value) => {
      managedLocal = value
    })
  })

  async function act(f: () => Promise<unknown>) {
    busy = true
    error = ''
    try {
      await f()
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      busy = false
    }
  }

  function connect() {
    return act(async () => {
      justResult = await api.nodeConnectProvider(
        nodeId,
        p.name,
        store.channels.filter((channel) => !channel.archived).map((channel) => channel.name),
        managedLocal || sameHostBrowser,
      )
    })
  }

  function openAgain() {
    if (!shownUrl) return
    let reserved
    try {
      reserved = prepareExternalOpen()
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
      return
    }
    return act(() => openExternal(shownUrl, reserved))
  }

  function paste() {
    const completionText = code.trim()
    if (!completionText) return
    return act(async () => {
      await api.nodeProviderCode(nodeId, p.name, completionText)
      code = ''
    })
  }

  function disconnect() {
    return act(async () => {
      await api.nodeDisconnectProvider(nodeId, p.name)
      justResult = null
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
    {providerLabel(p.name)}
    <small
      >{#if p.state === 'connected'}{p.kind === 'oauth' ? 'subscription' : 'api key'}{#if p.identity}
          · {p.identity}{/if}{:else if shownState === 'pending'}waiting on you{:else if p.state === 'failed'}failed{:else}not
        connected{/if}</small
    >
  </span>
  <span class="pst">
    {#if p.state === 'connected'}
      <span class="l"><span class="chip">connected</span>{#if p.channels.length} · {p.channels.join(', ')}{/if}{#if expiry()} · {expiry()}{/if}</span>
      {#if isSelf}
        <span><button class="lnk d" onclick={disconnect} disabled={busy}>Disconnect</button></span>
      {:else}
        <span class="l off">Manage on that node.</span>
      {/if}
    {:else if shownState === 'pending'}
      {#if shownUrl}
        <span class="l warn"><span class="chip warn">connect</span> · {completionInstruction(completion)}</span>
        {#if completionNote}<span class="l off">{completionNote}</span>{/if}
        {#if completion === 'device_code' && deviceCode}
          <span class="l">Enter code <code>{deviceCode}</code> at the provider page.</span>
        {/if}
        <span class="actions">
          <button class="lnk" onclick={openAgain} disabled={busy}>Open sign-in</button>
          <button class="lnk d" onclick={disconnect} disabled={busy}>Cancel</button>
        </span>
        {#if completion === 'local_callback'}
          <details class="alternate">
            <summary>Paste the redirect instead</summary>
            <span class="paste">
              <input
                aria-label="Redirect URL or code"
                placeholder="redirect URL or code"
                bind:value={code}
                onkeydown={(event) => event.key === 'Enter' && paste()}
              />
              <button class="btn p" onclick={paste} disabled={busy || !code.trim()}>Paste back</button>
            </span>
          </details>
        {:else}
          <span class="paste">
            <input
              aria-label="Redirect URL or code"
              placeholder="redirect URL or code"
              bind:value={code}
              onkeydown={(event) => event.key === 'Enter' && paste()}
            />
            <button class="btn p" onclick={paste} disabled={busy || !code.trim()}>Paste back</button>
          </span>
        {/if}
      {:else}
        <span class="l warn"><span class="chip warn">connect</span> · This sign-in belongs to your connection to that node.</span>
        <span><button class="lnk" onclick={connect} disabled={busy}>Resume sign-in</button></span>
      {/if}
    {:else}
      <span class="l off" class:bad={p.state === 'failed'}
        ><span class="chip" class:off={p.state !== 'failed'} class:bad={p.state === 'failed'}>{p.state === 'failed' ? 'failed' : 'disconnected'}</span>{#if p.error} · {p.error}{/if}</span
      >
      {#if p.can_login}
        {#if mayClaimBrowser}
          <label class="local-choice">
            <input type="checkbox" bind:checked={sameHostBrowser} />
            This browser is on the node
          </label>
        {/if}
        <span><button class="lnk" onclick={connect} disabled={busy}>{p.state === 'failed' ? 'Try again' : 'Connect'}</button></span>
      {:else}
        <span>API key only. Add it in <a class="lnk" href="/settings#credentials">Settings</a>.</span>
      {/if}
    {/if}
    {#if error}<span class="l bad" role="alert">{error}</span>{/if}
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
    gap: 5px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    min-width: 0;
  }
  .pst > span {
    white-space: normal;
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
  .actions,
  .paste {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .paste input {
    flex: 1;
    min-width: 12rem;
    font: 12.5px var(--mono);
    background: var(--s2);
    color: var(--ink);
    border: 0;
    border-radius: 3px;
    padding: 6px 8px;
  }
  .alternate {
    color: var(--dim);
  }
  .alternate summary {
    cursor: pointer;
    width: fit-content;
  }
  .alternate .paste {
    margin-top: 7px;
  }
  .local-choice {
    display: flex;
    align-items: center;
    gap: 7px;
    color: var(--dim);
    width: fit-content;
  }
  @media (max-width: 700px) {
    .prov {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 4px 12px;
    }
    .pst {
      grid-column: 2;
    }
    .paste input {
      flex-basis: 100%;
      min-width: 0;
      font-size: 16px;
    }
  }
</style>
