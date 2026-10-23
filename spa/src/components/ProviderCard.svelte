<script lang="ts">
  import { onMount, untrack } from 'svelte'
  import ChannelChoice from './ChannelChoice.svelte'
  import { api } from '../lib/api'
  import { providerChannelSeed } from '../lib/channel'
  import { clock } from '../lib/clock.svelte'
  import {
    browserCanClaimNode,
    desktopManagedLocal,
    openExternal,
    prepareExternalOpen,
  } from '../lib/external'
  import { formatDuration } from '../lib/format'
  import { completionInstruction, displacedSignIn, oneSignIn, providerLabel } from '../lib/providers'
  import { store } from '../lib/store.svelte'
  import {
    declareOps,
    fieldFor,
    modelOps,
    rebase,
    REASONING_EFFORTS,
    sectionOps,
    shown,
    type Clash,
    type Draft,
    type Field,
    type FieldContext,
    type SaveOutcome,
  } from '../lib/config-form'
  import ConfigField from './settings/ConfigField.svelte'
  import type { ConfigOp, ModelDecl, ProviderConfig, ProviderConnectResult, ProviderInfo } from '../lib/types'

  let {
    p,
    nodeId,
    config = null,
    editable = false,
    context = null,
    save,
  }: {
    p: ProviderInfo
    nodeId: string
    /** This provider's entry from `/api/config`: shape, upstream, and its
        declared models. Only ever passed for the serving node — a peer's
        node.toml is not something this browser reads directly, so its cards
        show no meta line and no declared-models section. */
    config?: ProviderConfig | null
    editable?: boolean
    /** node.toml as the page read it, for this provider's keys. */
    context?: FieldContext | null
    /** Send per-key operations on node.toml (`PATCH /api/config`). */
    save?: (ops: ConfigOp[], keys: string[]) => Promise<SaveOutcome>
  } = $props()

  let code = $state('')
  let apiKey = $state('')
  let busy = $state(false)
  let error = $state('')
  let justResult = $state<ProviderConnectResult | null>(null)
  let managedLocal = $state(false)
  let sameHostBrowser = $state(false)
  let shareSignIn = $state(true)
  // The operator's pick, once they touch the boxes; until then the seed.
  let chosen = $state<string[] | null>(null)
  let editingChannels = $state(false)
  // A sign-in or disconnect that takes the provider from channels, waiting
  // for the operator to confirm what it names.
  let confirming = $state<{ kind: 'connect' | 'disconnect'; text: string } | null>(null)

  // The declared-models editor's own local copy: edited freely, saved on
  // request. Re-seeded whenever the node's own config changes under it
  // (a fresh load elsewhere on the page, or this card's own save landing).
  // `config` is the page's `$state`, so its models are a proxy that
  // `structuredClone` refuses; a snapshot is the plain copy. Each row keeps
  // the index it was loaded at, so a save edits that model where it is.
  let models = $state<ModelDecl[]>([])
  let origins = $state<(number | null)[]>([])
  let modelsBusy = $state(false)
  let modelsError = $state('')
  let modelsSaved = $state('')
  $effect(() => {
    const loaded = config ? $state.snapshot(config.models) : []
    models = loaded
    origins = loaded.map((_, i) => i)
    modelsSaved = ''
  })
  const modelsValid = $derived.by(() => {
    const ids = models.map((m) => m.id.trim())
    if (ids.some((id) => !id)) return false
    return new Set(ids).size === ids.length
  })
  const modelRows = $derived(models.map((model, i) => ({ origin: origins[i] ?? null, model: { ...model } })))
  const modelEdits = $derived(context ? modelOps(context, p.name, modelRows) : [])
  const modelsDirty = $derived(modelEdits.length > 0)

  function addModel() {
    models.push({ id: '', name: '', reasoning: true })
    origins.push(null)
  }
  function removeModel(index: number) {
    models.splice(index, 1)
    origins.splice(index, 1)
  }

  function said(outcome: SaveOutcome): string {
    if (outcome.kind === 'saved') return ''
    if (outcome.kind === 'conflict') return 'node.toml changed on the node since this page read it; it has been read again. Check and save again.'
    if (outcome.kind === 'refused') return [...outcome.general, ...Object.values(outcome.fields).flat()].join(' ') || outcome.message
    return outcome.message
  }

  async function saveDeclaredModels() {
    if (!save || !modelsValid || modelsBusy || !modelsDirty) return
    modelsBusy = true
    modelsError = ''
    modelsSaved = ''
    const outcome = await save(modelEdits, [])
    modelsBusy = false
    if (outcome.kind === 'saved') {
      modelsSaved = outcome.result.restart_required ? 'Saved to node.toml; new sessions see it once the node restarts.' : 'Saved to node.toml.'
    } else {
      modelsError = said(outcome)
    }
  }

  // The provider's own keys: where it is reached and with what.
  const ENDPOINT = ['upstream', 'shape', 'credential', 'login', 'price.input_per_mtok', 'price.output_per_mtok']
  const endpoint = $derived(
    context ? ENDPOINT.map((name) => fieldFor(context, ['providers', p.name, ...name.split('.')])).filter((f): f is Field => f !== null) : [],
  )
  let endpointDrafts = $state<Record<string, Draft>>({})
  let endpointErrors = $state<Record<string, string[]>>({})
  let endpointNote = $state('')
  let endpointSaved = $state(false)
  let endpointBusy = $state(false)
  let endpointClashes = $state<Clash[]>([])
  // node.toml read again (this card's save, another card's, a conflict):
  // the drafts are carried onto it key by key, and a key the node changed
  // under one is named before it can be saved over.
  let endpointSeen: Field[] = []
  $effect(() => {
    const now = endpoint
    untrack(() => {
      if (endpointSeen.length && Object.keys(endpointDrafts).length) {
        const carried = rebase(endpointSeen, now, endpointDrafts)
        endpointDrafts = carried.drafts
        const kept = endpointClashes.filter((c) => !carried.clashes.some((n) => n.key === c.key))
        endpointClashes = [...kept, ...carried.clashes]
      }
      endpointSeen = now
    })
  })
  function endpointTheirs() {
    for (const clash of endpointClashes) delete endpointDrafts[clash.key]
    endpointClashes = []
  }
  const endpointPlan = $derived(sectionOps(endpoint, endpointDrafts))
  const endpointDirty = $derived(endpointPlan.ops.length + Object.keys(endpointPlan.errors).length > 0)

  async function saveEndpoint() {
    if (!save || !context || endpointBusy || endpointClashes.length || !endpointPlan.ops.length || Object.keys(endpointPlan.errors).length) return
    endpointBusy = true
    endpointNote = ''
    endpointErrors = {}
    const keys = endpoint.map((f) => f.key)
    const ops = [...declareOps(context, p.name, endpointPlan.ops.map((o) => ('key' in o ? o.key : ''))), ...endpointPlan.ops]
    const outcome = await save(ops, keys)
    endpointBusy = false
    endpointSaved = outcome.kind === 'saved'
    if (outcome.kind === 'saved') {
      endpointDrafts = {}
      endpointNote = outcome.result.restart_required ? 'Saved; the node uses it once it restarts.' : 'Saved.'
    } else if (outcome.kind === 'refused') {
      endpointErrors = outcome.fields
      endpointNote = outcome.general.join(' ') || 'Refused: see the keys marked.'
    } else {
      endpointNote = said(outcome)
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
  const channelChoices = $derived.by(() => {
    const open = store.channels.filter((channel) => !channel.archived)
    const member = open.filter((channel) => channel.nodes.includes(nodeId))
    return member.length ? member : open
  })
  const picked = $derived(
    chosen ??
      providerChannelSeed({
        existing: p.channels,
        nodeDefault: store.nodes.find((node) => node.id === nodeId)?.default_channel,
        channels: store.channels,
        nodeId,
      }),
  )

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

  const peerName = (id: string) => store.nodes.find((node) => node.id === id)?.name ?? id.slice(0, 8)

  // The serving node holds one sign-in per provider: one that would take it
  // from channels is confirmed first, naming them. A peer's card has no
  // such list to name, and the peer refuses replacing what its sender
  // does not share.
  function connect(confirmed = false) {
    const text = isSelf && !confirmed ? displacedSignIn(p, picked, peerName) : null
    confirming = text ? { kind: 'connect', text } : null
    if (text) return
    return act(async () => {
      justResult = await api.nodeConnectProvider(
        nodeId,
        p.name,
        picked,
        managedLocal || sameHostBrowser,
        shareSignIn,
        confirmed,
      )
    })
  }

  function editChannels() {
    chosen = [...p.channels]
    editingChannels = true
  }

  function cancelChannels() {
    chosen = null
    editingChannels = false
  }

  function saveChannels() {
    return act(async () => {
      await api.setProviderChannels(p.name, picked)
      chosen = null
      editingChannels = false
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

  function saveKey() {
    const key = apiKey.trim()
    if (!key) return
    return act(async () => {
      await api.setProviderKey(p.name, key, picked)
      apiKey = ''
      chosen = null
    })
  }

  // Go ahead with what the operator was shown. If what it would take has
  // changed since (other channels picked, a fresher listing), the new text
  // is what they confirm instead.
  function goAhead() {
    if (!confirming) return
    const { kind, text } = confirming
    const now = displacedSignIn(p, kind === 'connect' ? picked : null, peerName)
    if (now !== null && now !== text) {
      confirming = { kind, text: now }
      return
    }
    return kind === 'connect' ? connect(true) : disconnect(true)
  }

  function disconnect(confirmed = false) {
    const text = isSelf && !confirmed ? displacedSignIn(p, null, peerName) : null
    confirming = text ? { kind: 'disconnect', text } : null
    if (text) return
    return act(async () => {
      await api.nodeDisconnectProvider(nodeId, p.name, confirmed)
      justResult = null
    })
  }


  function expiry(): string {
    if (!p.expires_ms) return ''
    const left = p.expires_ms - clock.now
    return left <= 0 ? 'expired' : `refreshes in ${formatDuration(left)}`
  }
</script>

<div
  class="prov"
  class:pending={shownState === 'pending'}
  class:bad={p.state === 'failed'}
  class:off={shownState === 'disconnected'}
>
  <span class="pbar"></span>
  <!-- What kind of connection this is; the state is the chip beside it. -->
  <span class="pnm">
    {providerLabel(p.name)}
    <small
      >{#if p.state === 'connected'}{p.kind === 'oauth' ? 'subscription' : 'api key'}{#if p.identity}{' · '}{p.identity}{/if}{:else if shownState === 'pending'}waiting on you{:else}{p.can_login
          ? 'subscription'
          : 'api key'}{/if}</small
    >
  </span>
  <span class="pst">
    {#if !isSelf}
      <span class="scope">Remote node {nodeName}: commands are sealed to it. Its provider credential stays there and this browser cannot claim its local callback.</span>
    {/if}
    {#if p.state === 'connected'}
      <span class="l"
        ><span class="chip">connected</span>{#if p.channels.length}{' · '}{p.channels.join(', ')}{:else}{' · '}no channel{/if}{#if isSelf && editable && !editingChannels}
          · <button class="lnk" onclick={editChannels} disabled={busy}>edit channels</button>{/if}</span
      >
      {#if editingChannels}
        <ChannelChoice
          legend="Channels this sign-in serves"
          choices={channelChoices}
          selected={picked}
          onchange={(next) => (chosen = next)}
          disabled={busy}
        />
        {#if p.channels.some((channel) => !picked.includes(channel)) && store.nodes.length > 1}
          <span class="l off">Peers that already hold this sign-in keep it for channels you remove.</span>
        {/if}
        <span class="actions">
          <button class="btn p" onclick={saveChannels} disabled={busy || picked.length === 0}>{busy ? 'Saving…' : 'Save channels'}</button>
          <button class="lnk" onclick={cancelChannels} disabled={busy}>Cancel</button>
        </span>
      {/if}
      {#if isSelf && p.kind === 'oauth'}
        <span class="l off">{oneSignIn(p)}</span>
      {/if}
      {#if isSelf}
        <span><button class="lnk d" onclick={() => disconnect()} disabled={busy}>Disconnect</button></span>
      {:else}
        <span><button class="lnk d" onclick={() => disconnect()} disabled={busy}>Disconnect on {nodeName}</button></span>
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
                <span>Model id</span><span>Name</span><span>Context</span><span>Output</span><span>Reasons</span><span>Attach</span><span>Parallel</span><span>Effort</span><span></span>
              </div>
              {#each models as m, i (i)}
                <div class="mrow">
                  <input bind:value={m.id} placeholder="the provider's model id" aria-label="Model id" disabled={!editable} spellcheck="false" />
                  <input bind:value={m.name} placeholder="shown in the picker" aria-label="Model name" disabled={!editable} spellcheck="false" />
                  <input type="number" min="0" bind:value={m.context} placeholder="default" aria-label="Context tokens" disabled={!editable} />
                  <input type="number" min="0" bind:value={m.output} placeholder="default" aria-label="Output tokens" disabled={!editable} />
                  <input type="checkbox" bind:checked={m.reasoning} aria-label="Reasoning" disabled={!editable} />
                  <input type="checkbox" bind:checked={m.attachment} aria-label="Attachments" disabled={!editable} />
                  <input type="checkbox" checked={m.parallel_tools !== false} onchange={(e) => (m.parallel_tools = e.currentTarget.checked)} aria-label="Parallel tool calls" disabled={!editable} />
                  <select bind:value={m.reasoning_effort} aria-label="Reasoning effort" disabled={!editable}>
                    <option value="">default</option>
                    {#each REASONING_EFFORTS as effort (effort)}<option value={effort}>{effort}</option>{/each}
                  </select>
                  <button class="lnk" type="button" onclick={() => removeModel(i)} disabled={!editable} aria-label={`Remove ${m.id || 'model'}`}>Remove</button>
                </div>
              {/each}
            {:else}
              <div class="empty">No models declared: nothing is offered under {p.name}.</div>
            {/if}
            {#if editable && save}
              <div class="model-acts">
                <button class="lnk" type="button" onclick={addModel} disabled={modelsBusy}>+ Add model</button>
                {#if modelsDirty}
                  <button class="btn p" type="button" onclick={saveDeclaredModels} disabled={!modelsValid || modelsBusy}>
                    {modelsBusy ? 'Saving…' : 'Save models'}
                  </button>
                {/if}
                {#if modelsDirty && !modelsValid}<small class="bad">Every model needs an id, and each id once.</small>
                {:else if modelsSaved}<small class="ok">{modelsSaved}</small>
                {:else if modelsError}<small class="bad">{modelsError}</small>{/if}
              </div>
            {/if}
          </div>
        </details>
        {#if endpoint.length && editable && save}
          <details class="declared" open={endpointDirty || !!endpointNote}>
            <summary>Endpoint · <code>[providers.{p.name}]</code></summary>
            {#if endpointClashes.length}
              <div class="clash" role="alert">
                <b>Changed on the node too</b>
                {#each endpointClashes as clash (clash.key)}
                  <small><code>{clash.key}</code>: now {shown(clash.theirs)} on the node; saving writes {shown(clash.yours.reset ? undefined : clash.yours.v)}.</small>
                {/each}
                <span class="model-acts">
                  <button class="btn" type="button" onclick={() => (endpointClashes = [])}>Keep mine</button>
                  <button class="lnk" type="button" onclick={endpointTheirs}>Use theirs</button>
                </span>
              </div>
            {/if}
            <div class="endpoint">
              {#each endpoint as field (field.key)}
                <ConfigField
                  {field}
                  draft={endpointDrafts[field.key]}
                  errors={endpointErrors[field.key] ?? []}
                  disabled={endpointBusy}
                  onchange={(d) => {
                    if (d) endpointDrafts[field.key] = d
                    else delete endpointDrafts[field.key]
                  }}
                />
              {/each}
            </div>
            <div class="model-acts">
              {#if endpointDirty}
                <button class="btn p" type="button" onclick={saveEndpoint} disabled={endpointBusy || endpointClashes.length > 0 || Object.keys(endpointPlan.errors).length > 0}>
                  {endpointBusy ? 'Saving…' : 'Save endpoint'}
                </button>
                <button class="lnk" type="button" onclick={() => ((endpointDrafts = {}), (endpointClashes = []))} disabled={endpointBusy}>Discard</button>
              {/if}
              {#if endpointNote}<small class={endpointSaved ? 'ok' : 'bad'} role="status">{endpointNote}</small>{/if}
            </div>
          </details>
        {/if}
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
          <button class="lnk d" onclick={() => disconnect()} disabled={busy}>Cancel</button>
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
        <span><button class="lnk" onclick={() => connect()} disabled={busy}>Resume sign-in</button></span>
      {/if}
    {:else}
      <span class="l off" class:bad={p.state === 'failed'}
        ><span class="chip" class:off={p.state !== 'failed'} class:bad={p.state === 'failed'}>{p.state === 'failed' ? 'failed' : 'disconnected'}</span>{#if p.error}{' · '}{p.error}{/if}</span
      >
      {#if p.can_login}
        {#if channelChoices.length > 1}
          <ChannelChoice
            legend="Channels this sign-in serves"
            choices={channelChoices}
            selected={picked}
            onchange={(next) => (chosen = next)}
            disabled={busy}
          />
        {/if}
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
        {#if isSelf}<span class="l off">{oneSignIn(p)}</span>{/if}
        <span><button class="lnk" onclick={() => connect()} disabled={busy || picked.length === 0}>{p.state === 'failed' ? 'Try again' : 'Connect'}</button></span>
      {:else if isSelf}
        {#if channelChoices.length > 1}
          <ChannelChoice
            legend="Channels this key serves"
            choices={channelChoices}
            selected={picked}
            onchange={(next) => (chosen = next)}
            disabled={busy}
          />
        {/if}
        <span class="paste">
          <input
            type="password"
            autocomplete="off"
            aria-label={`API key for ${providerLabel(p.name)}`}
            placeholder={p.state === 'failed' ? 'a new API key' : 'API key'}
            bind:value={apiKey}
            onkeydown={(event) => event.key === 'Enter' && saveKey()}
            disabled={busy}
          />
          <button class="btn p" onclick={saveKey} disabled={busy || !apiKey.trim() || picked.length === 0}
            >{busy ? 'Saving…' : 'Save key'}</button
          >
        </span>
      {:else}
        <span>Add the key while managing {nodeName}; it never crosses the mesh to this browser.</span>
      {/if}
    {/if}
    {#if confirming}
      <span class="confirm" role="alert">
        <span class="l warn">{confirming.text}</span>
        <span class="actions">
          <button
            class="btn d"
            onclick={goAhead}
            disabled={busy}>{confirming.kind === 'connect' ? 'Sign in anyway' : 'Disconnect anyway'}</button
          >
          <button class="lnk" onclick={() => (confirming = null)} disabled={busy}>Cancel</button>
        </span>
      </span>
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
  .prov.off .pbar {
    background: var(--dim);
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
  .confirm {
    display: flex;
    flex-direction: column;
    gap: 5px;
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
    grid-template-columns: minmax(9rem, 2fr) minmax(7rem, 2fr) 5.5rem 5.5rem 4rem 4rem 4.5rem 6rem auto;
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
  .clash {
    display: grid;
    gap: 4px;
    margin-top: 6px;
    padding: 6px 10px;
    background: var(--wash-wait);
    border-left: 3px solid var(--wait);
    overflow-wrap: anywhere;
  }
  .endpoint {
    display: grid;
    gap: 8px;
    grid-template-columns: repeat(auto-fill, minmax(min(100%, 240px), 1fr));
    margin-top: 6px;
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
      grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    }
    .mrow input:not([type='checkbox']) {
      min-width: 0;
    }
    .mrow.mhead {
      display: none;
    }
  }
</style>
