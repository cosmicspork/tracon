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
  import type { ModelDecl, ProviderConfig, ProviderConnectResult, ProviderInfo } from '../lib/types'

  let {
    p,
    nodeId,
    config = null,
    editable = false,
    saveModels,
  }: {
    p: ProviderInfo
    nodeId: string
    /** This provider's entry from `/api/config`: shape, upstream, and its
        declared models. Only ever passed for the serving node — a peer's
        node.toml is not something this browser reads directly, so its cards
        show no meta line and no declared-models section. */
    config?: ProviderConfig | null
    editable?: boolean
    saveModels?: (name: string, models: ModelDecl[]) => Promise<unknown>
  } = $props()

  let code = $state('')
  let busy = $state(false)
  let error = $state('')
  let justResult = $state<ProviderConnectResult | null>(null)
  let managedLocal = $state(false)
  let sameHostBrowser = $state(false)
  let shareSignIn = $state(true)

  // The declared-models editor's own local copy: edited freely, saved on
  // request. Re-seeded whenever the node's own config changes under it
  // (a fresh load elsewhere on the page, or this card's own save landing).
  let models = $state<ModelDecl[]>([])
  let modelsBusy = $state(false)
  let modelsError = $state('')
  let modelsSaved = $state(false)
  $effect(() => {
    models = config ? structuredClone(config.models) : []
    modelsSaved = false
  })
  const modelsValid = $derived.by(() => {
    const ids = models.map((m) => m.id.trim())
    if (ids.some((id) => !id)) return false
    return new Set(ids).size === ids.length
  })
  const modelsDirty = $derived(config ? JSON.stringify(config.models) !== JSON.stringify(models) : false)

  function addModel() {
    models.push({ id: '', name: '', reasoning: true })
  }
  function removeModel(index: number) {
    models.splice(index, 1)
  }
  async function saveDeclaredModels() {
    if (!saveModels || !modelsValid || modelsBusy) return
    modelsBusy = true
    modelsError = ''
    modelsSaved = false
    try {
      await saveModels(p.name, models)
      modelsSaved = true
    } catch (e) {
      modelsError = e instanceof Error ? e.message : String(e)
    } finally {
      modelsBusy = false
    }
  }

  const isSelf = $derived(nodeId === store.node?.id)
  const nodeName = $derived(store.nodes.find((node) => node.id === nodeId)?.name ?? nodeId.slice(0, 8))
  const mayClaimBrowser = $derived(isSelf && !managedLocal && browserCanClaimNode())
  const meshed = $derived(store.nodes.length > 1)
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
        shareSignIn,
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
    {#if !isSelf}
      <span class="scope">Remote node {nodeName}: commands are sealed to it. Its provider credential stays there and this browser cannot claim its local callback.</span>
    {/if}
    {#if p.state === 'connected'}
      <span class="l"><span class="chip">connected</span>{#if p.channels.length} · {p.channels.join(', ')}{/if}</span>
      {#if isSelf}
        <span><button class="lnk d" onclick={disconnect} disabled={busy}>Disconnect</button></span>
      {:else}
        <span><button class="lnk d" onclick={disconnect} disabled={busy}>Disconnect on {nodeName}</button></span>
      {/if}
      {#if config}
        <span class="meta">
          {#if p.kind === 'oauth'}
            {expiry() ? `Renews automatically · ${expiry()}` : 'Renews automatically'}
          {:else}
            {config.shape}-compatible · {config.upstream}
          {/if}
        </span>
        <details class="declared" open={models.length > 0 && models.length <= 4}>
          <summary>Declared models · {models.length}</summary>
          <div class="models">
            {#if models.length}
              <div class="mrow mhead" aria-hidden="true">
                <span>Model id</span><span>Name</span><span>Context</span><span>Output</span><span>Reasons</span><span>Attach</span><span></span>
              </div>
              {#each models as m, i (i)}
                <div class="mrow">
                  <input bind:value={m.id} placeholder="the provider's model id" aria-label="Model id" disabled={!editable} spellcheck="false" />
                  <input bind:value={m.name} placeholder="shown in the picker" aria-label="Model name" disabled={!editable} spellcheck="false" />
                  <input type="number" min="0" bind:value={m.context} placeholder="default" aria-label="Context tokens" disabled={!editable} />
                  <input type="number" min="0" bind:value={m.output} placeholder="default" aria-label="Output tokens" disabled={!editable} />
                  <input type="checkbox" bind:checked={m.reasoning} aria-label="Reasoning" disabled={!editable} />
                  <input type="checkbox" bind:checked={m.attachment} aria-label="Attachments" disabled={!editable} />
                  <button class="lnk" type="button" onclick={() => removeModel(i)} disabled={!editable} aria-label={`Remove ${m.id || 'model'}`}>Remove</button>
                </div>
              {/each}
            {:else}
              <div class="empty">No models declared: nothing is offered under {p.name}.</div>
            {/if}
            {#if editable}
              <div class="model-acts">
                <button class="lnk" type="button" onclick={addModel} disabled={modelsBusy}>+ Add model</button>
                {#if modelsDirty}
                  <button class="btn p" type="button" onclick={saveDeclaredModels} disabled={!modelsValid || modelsBusy}>
                    {modelsBusy ? 'Saving…' : 'Save models'}
                  </button>
                {/if}
                {#if modelsDirty && !modelsValid}<small class="bad">Every model needs an id, and each id once.</small>
                {:else if modelsSaved}<small class="ok">Saved to node.toml; restart the node for new sessions to see it.</small>
                {:else if modelsError}<small class="bad">{modelsError}</small>{/if}
              </div>
            {/if}
          </div>
        </details>
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
        {:else if completion !== 'device_code'}
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
            This browser is physically on the serving node; use its local callback
          </label>
        {/if}
        {#if meshed}
          <label class="local-choice">
            <input type="checkbox" bind:checked={shareSignIn} />
            Share this sign-in with every node in its channels
          </label>
        {/if}
        <span><button class="lnk" onclick={connect} disabled={busy}>{p.state === 'failed' ? 'Try again' : 'Connect'}</button></span>
      {:else if isSelf}
        <span>API key only. Add it under <a class="lnk" href="/settings#connections">serving-node credentials</a>.</span>
      {:else}
        <span>API key only. Add the key while managing {nodeName}; it never crosses the mesh to this browser.</span>
    {/if}
    {/if}
    {#if error}<span class="l bad" role="alert">{error}</span>{/if}
  </span>
</div>

<style>
  .prov {
    display: grid;
    grid-template-columns: 3px 140px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
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
    background: linear-gradient(90deg, var(--wash-wait), var(--s2) 42%);
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
  .scope {
    color: var(--dim);
    font: 11.5px var(--mono);
    white-space: normal;
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
    background: var(--s1);
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
  .meta {
    color: var(--ink2);
  }
  .declared {
    border-top: 1px solid var(--rule);
    margin-top: 2px;
    padding-top: 8px;
  }
  .declared > summary {
    cursor: pointer;
    font: 500 12.5px var(--sans);
    color: var(--ink2);
    list-style: none;
    display: flex;
    align-items: center;
    gap: 6px;
    width: fit-content;
  }
  .declared > summary::-webkit-details-marker {
    display: none;
  }
  .declared > summary::before {
    content: '▸';
    font-size: 10px;
    color: var(--dim);
  }
  .declared[open] > summary::before {
    content: '▾';
  }
  .models {
    display: grid;
    gap: 0.3rem;
    margin-top: 8px;
  }
  .mrow {
    display: grid;
    grid-template-columns: minmax(9rem, 2fr) minmax(7rem, 2fr) 5.5rem 5.5rem 4rem 5rem auto;
    gap: 0.4rem;
    align-items: center;
  }
  .mrow.mhead {
    font-size: 0.8em;
    opacity: 0.7;
  }
  .mrow input[type='checkbox'] {
    justify-self: start;
  }
  .model-acts {
    display: flex;
    align-items: center;
    gap: 10px 14px;
    flex-wrap: wrap;
    margin-top: 2px;
  }
  .model-acts .ok {
    color: var(--ok);
  }
  .model-acts .bad {
    color: var(--crit);
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
    .mrow {
      grid-template-columns: 1fr 1fr;
    }
    .mrow.mhead {
      display: none;
    }
  }
</style>
