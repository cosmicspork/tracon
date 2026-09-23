<script lang="ts">
  // A plain session is the default: its first message is queued after the
  // harness starts. Creating durable work items and plans is explicit.
  import ModelPicker from './ModelPicker.svelte'
  import RepoPicker from './RepoPicker.svelte'
  import { api, ApiError } from '../lib/api'
  import { modelLabel, phaseDefaults } from '../lib/bindings'
  import { defaultChannel, rememberChannel, rememberedChannel } from '../lib/channel'
  import { digits, formatGrouped, formatTokens } from '../lib/format'
  import { recentModelValues } from '../lib/models'
  import { eligibleNodes, modelsForChannel, nodeReadiness } from '../lib/nodes'
  import { repoLabel } from '../lib/repo'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { NodeInfo, WorkView } from '../lib/types'

  let {
    item = null,
    phase = $bindable('execute'),
    preferredNodeId = null,
    preferredChannel = null,
    ontargetchange = undefined,
  }: {
    item?: WorkView | null
    phase?: 'plan' | 'execute'
    preferredNodeId?: string | null
    preferredChannel?: string | null
    ontargetchange?: (target: { nodeId: string | null; channel: string }) => void
  } = $props()

  let prompt = $state('')
  let channel = $state('')
  let repo = $state('')
  let modelScope = $state<string | null>(null)
  let workspaceId = $state<string | null>(null)
  let repoScope = $state<string | null>(null)
  let branch = $state('')
  let model = $state('')
  let budget = $state('')
  let nodeId = $state<string | null>(null)
  let open = $state(false)
  let busy = $state(false)
  let error = $state<string | null>(null)
  let savedItem = $state<string | null>(null)
  let structured = $state(false)
  $effect(() => {
    if (item) structured = true
  })

  // An archived channel takes no new sessions, so it is not offered.
  const channelNames = $derived(store.channels.filter((c) => !c.archived).map((c) => c.name))
  const channelInfo = $derived(store.channels.find((c) => c.name === channel))
  const memberships = $derived(Object.fromEntries(store.channels.map((c) => [c.name, c.nodes])))
  const eligible = $derived(
    eligibleNodes(store.nodes, memberships, channel).filter((node) => modelsForChannel(node, channel, store.providers, channelInfo?.bindings).length > 0),
  )
  const selectedNode = $derived(
    eligible.find((node) => node.id === nodeId) ?? eligible.find((node) => node.is_self) ?? eligible[0] ?? null,
  )
  // `node` keeps the operator's own row visible when no target is usable;
  // only `selectedNode` is ever sent to the API.
  const node = $derived(selectedNode ?? store.node)
  const atCeiling = $derived(channelInfo?.ceiling.state === 'at')
  const blocked = $derived(selectedNode === null)
  const sessionPhase = $derived(structured ? phase : 'execute')
  const bound = $derived(phaseDefaults(channelInfo?.bindings, sessionPhase))
  const models = $derived(selectedNode ? modelsForChannel(selectedNode, channel, store.providers, channelInfo?.bindings) : [])
  const recentModels = $derived(recentModelValues(store.sessions.values()))
  const planLabel = $derived(modelLabel(phaseDefaults(channelInfo?.bindings, 'plan').model, models))
  const execLabel = $derived(modelLabel(phaseDefaults(channelInfo?.bindings, 'execute').model, models))
  const needsPlan = $derived(structured && phase === 'execute' && item !== null && !item.phase_plan_slug)
  const ready = $derived(
    !blocked &&
      !atCeiling &&
      !needsPlan &&
      channel !== '' &&
      (repo.trim() !== '' || workspaceId !== null) &&
      (item !== null || prompt.trim() !== '') &&
      (model === '' || models.some((candidate) => candidate.value === model)) &&
      !busy,
  )
  function nodeBlock(node: NodeInfo): string | null {
    const members = memberships[channel]
    if (members && !members.includes(node.id)) return `Not a member of ${channel}.`
    const readiness = nodeReadiness(node)
    if (!readiness.canRun) return readiness.detail
    if (modelsForChannel(node, channel, store.providers, channelInfo?.bindings).length === 0)
      return `No connected provider offers a model for ${channel}.`
    return null
  }
  function selectLocalRepository(selection: { targetId: string; channel: string; repo: string; workspaceId: string | null }) {
    if (selectedNode?.id !== selection.targetId || !selectedNode.is_self || channel !== selection.channel) return
    repo = selection.repo
    workspaceId = selection.workspaceId
  }


  $effect(() => {
    if (preferredChannel !== null) channel = preferredChannel
    if (preferredNodeId !== null) nodeId = preferredNodeId
  })
  $effect(() => {
    ontargetchange?.({ nodeId: selectedNode?.id ?? null, channel })
  })
  $effect(() => {
    const scope = selectedNode ? `${selectedNode.id}:${channel}:${sessionPhase}` : null
    if (scope === modelScope) return
    modelScope = scope
    model = ''
    budget = ''
  })
  $effect(() => {
    if (model && !models.some((candidate) => candidate.value === model)) model = ''
  })
  // A workspace import belongs to the serving node and cannot be forwarded to
  // a peer. Do not carry a local checkout into a different runner either: a
  // remote path is only used after the operator enters it for that machine.
  $effect(() => {
    const scope = selectedNode ? `${selectedNode.id}:${channel}` : null
    if (scope === repoScope) return
    repoScope = scope
    workspaceId = null
    repo = ''
  })


  // The channel the node actually has; the item's own channel when there is one.
  $effect(() => {
    if (item) {
      channel = item.channel
      return
    }
    if (!channel && channelNames.length) {
      channel = defaultChannel({
        names: channelNames,
        remembered: rememberedChannel(),
        nodeDefault: store.node?.default_channel,
        sessions: [...store.sessions.values()].filter((s) => s.node_id === store.node?.id),
      })
    }
  })
  // The repository this channel worked in last is a local convenience only.
  // A peer has its own filesystem, so it must receive an explicit path.
  $effect(() => {
    if (!node?.is_self || repo !== '' || workspaceId !== null || !channel) return
    const last = [...store.sessions.values()]
      .sort((a, b) => b.created_ms - a.created_ms)
      .find((s) => s.node_id === node.id && s.channel === channel && s.repo_path && !s.repo_path.startsWith('workspace://'))
    if (last) repo = last.repo_path
  })
  $effect(() => {
    if (bound.budget_tokens && !budget) budget = String(bound.budget_tokens)
  })

  async function start(e: SubmitEvent) {
    e.preventDefault()
    if (!ready) return
    busy = true
    error = null
    savedItem = null
    try {
      const common = {
        channel,
        repo_path: workspaceId ? '' : repo.trim(),
        workspace_id: workspaceId ?? undefined,
        branch: branch.trim() || undefined,
        phase: sessionPhase,
        model: model || undefined,
        budget_tokens: budget === '' ? undefined : Number(budget),
        node_id: selectedNode && !selectedNode.is_self ? selectedNode.id : undefined,
      }
      const lines = prompt.trim().split('\n')
      const session = item
        ? await api.createSession({ ...common, work_item_id: item.id })
        : structured
          ? (await api.compose({ ...common, phase: 'plan', title: lines[0], body: lines.slice(1).join('\n').trim() })).session
          : await api.createSession({ ...common, initial_prompt: prompt.trim() })
      if (!item) rememberChannel(channel)
      await store.refetch()
      router.go(`/sessions/${session.id}`)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      if (e instanceof ApiError && e.workItemId) savedItem = e.workItemId
    } finally {
      busy = false
    }
  }

  function onkeydown(e: KeyboardEvent) {
    // Enter sends; a newline needs a modifier, as it does everywhere else a
    // message is typed.
    if (e.key === 'Enter' && !e.shiftKey && !e.altKey) {
      e.preventDefault()
      ;(e.currentTarget as HTMLElement).closest('form')?.requestSubmit()
    }
  }
</script>

<form class="comp" onsubmit={start}>
  {#if item}
    <div class="on-item">
      <span class="lbl">{phase === 'plan' ? 'Plan' : 'Execute'}</span>
      <span class="ttl">{item.title}</span>
      <a href="/work/{item.id}">open item</a>
    </div>
  {:else}
    <textarea
      bind:value={prompt}
      {onkeydown}
      rows="2"
      placeholder={structured ? 'What should this plan cover?' : 'What should get done?'}
      spellcheck="false"
      disabled={busy}
    ></textarea>
  {/if}

  <div class="line">
    <div class="ctx">
      {#if channel}<span>{channel}</span>{/if}
      {#if repo}<span title={repo}>{repoLabel(repo)}</span>{/if}
      {#if planLabel || execLabel}
        <span>{planLabel ? `plans on ${planLabel}` : 'no plan model'}{execLabel ? `, builds on ${execLabel}` : ''}</span>
      {/if}
      <button type="button" class="lnk" onclick={() => (open = !open)}>{open ? 'close' : 'adjust'}</button>
    </div>
    <button class="btn p" type="submit" disabled={!ready}>
      {#if busy}Starting…{:else if atCeiling}{channel} is at its ceiling{:else if structured}Start {item ? phase : 'plan'}{:else}Start session{/if}
    </button>
  </div>

  {#if open}
    <div class="adjust">
      <label>
        <span>Channel</span>
        <select bind:value={channel} disabled={item !== null}>
          {#each channelNames as c (c)}<option value={c}>{c}</option>{/each}
        </select>
        {#if channelInfo?.ceiling.ceiling}
          <small class:crit={atCeiling}
            >{formatTokens(channelInfo.ceiling.usage_today)} of {formatTokens(channelInfo.ceiling.ceiling)} tokens
            today{atCeiling ? ' · at its ceiling: new sessions are refused' : ''}</small
          >
        {/if}
      </label>
      {#if selectedNode && !selectedNode.is_self}
        <label>
          <span>Repository path on {selectedNode.name}</span>
          <input bind:value={repo} placeholder="Absolute path on {selectedNode.name}" spellcheck="false" />
          <small>This exact path is sent to {selectedNode.name}. Clones and browser-file uploads on {store.node?.name ?? 'this controller'} are not transferred to a peer.</small>
        </label>
      {:else if selectedNode?.is_self}
        <div class="field">
          <span>Repository</span>
          <RepoPicker
            bind:value={repo}
            bind:workspaceId
            {channel}
            targetId={selectedNode.id}
            onselect={selectLocalRepository}
          />
        </div>
      {:else}
        <div class="field">
          <span>Repository</span>
          <small class="crit">Choose a usable runner before selecting a repository.</small>
        </div>
      {/if}
      <label>
        <span>Branch</span>
        <input bind:value={branch} placeholder="feat/…  (a name is generated if empty)" spellcheck="false" />
      </label>
      {#if !item}
        <div class="field">
          <span>Workflow</span>
          <div class="seg" role="radiogroup">
            <button
              type="button"
              class:on={!structured}
              onclick={() => {
                structured = false
                phase = 'execute'
              }}>Plain session</button>
            <button
              type="button"
              class:on={structured}
              onclick={() => {
                structured = true
                phase = 'plan'
              }}>Plan work item</button>
          </div>
          <small>{structured ? 'Writes a durable work item, then runs its plan phase.' : 'Starts directly; no work item or plan is created.'}</small>
        </div>
      {/if}
      {#if item}
        <div class="field">
          <span>Phase</span>
          <div class="seg" role="radiogroup">
            <button type="button" class:on={phase === 'plan'} onclick={() => (phase = 'plan')}>Plan</button>
            <button type="button" class:on={phase === 'execute'} onclick={() => (phase = 'execute')}>Execute</button>
          </div>
          <small
            >{phase === 'plan'
              ? 'Reads, thinks, and ends by writing the plan document.'
              : "Does the work from the item's plan, then submits for review."}</small
          >
          {#if needsPlan}<small class="crit">This item has no plan yet: run a plan session first.</small>{/if}
        </div>
      {/if}
      <label>
        <span>Model <em>{bound.model ? `${channel} binds one to ${sessionPhase}` : 'automatic by default'}</em></span>
        <ModelPicker bind:value={model} {models} recent={recentModels} none="Automatic (channel or node default)" />
        {#if models.length === 0}
          <small class="crit">No model is available for this channel. Declare one on a provider and check its channel scope; refresh only if a declared model has not appeared.</small>
        {/if}
      </label>
      <label>
        <span>Budget <em>{budget.trim() === '' ? 'channel or node default' : Number(budget) ? `${formatTokens(Number(budget))} tokens` : 'no cap'}</em></span>
        <input
          value={budget ? formatGrouped(Number(budget)) : ''}
          placeholder="No cap"
          inputmode="numeric"
          spellcheck="false"
          oninput={(e) => {
            const raw = (e.currentTarget as HTMLInputElement).value
            budget = raw.replace(/\D/g, '') ? String(digits(raw)) : ''
          }}
        />
        <small>Optional cap across input, output, and cached-read tokens. A session is stopped only when a cap is set and reached.</small>
      </label>
      <div class="field">
        <span>Runs on</span>
        {#if store.nodes.length > 1}
          <div class="pick">
            {#each store.nodes as n (n.id)}
              {@const reason = nodeBlock(n)}
              {@const readiness = nodeReadiness(n)}
              <label class:no={reason !== null}>
                <input
                  type="radio"
                  name="node"
                  value={n.id}
                  disabled={reason !== null}
                  checked={selectedNode?.id === n.id}
                  onchange={() => (nodeId = n.id)}
                />
                <span class="chip" class:bad={!readiness.canRun} class:off={!n.reachable}>{n.name}</span>
                {#if reason}<small class="crit">{readiness.label} · {reason}</small>{/if}
              </label>
            {/each}
          </div>
        {:else if node && nodeBlock(node)}
          <span class="chip bad" class:off={!node.reachable}>{node.name}</span>
          <small class="crit">{nodeReadiness(node).label} · {nodeBlock(node)}</small>
        {:else if node}
          <span class="chip">{node.name}</span>
          <small>{node.harness.id} {node.harness.found ?? node.harness.pinned}</small>
        {:else}
          <small class="crit">No node has reported a usable isolated runtime for {channel || 'this channel'}.</small>
        {/if}
      </div>
    </div>
  {/if}

  {#if error}
    <div class="banner crit">
      could not start <b>· {error}</b>
      {#if savedItem}<i>What you typed is saved as a work item · <a href="/work/{savedItem}">open it</a></i>{/if}
    </div>
  {/if}
</form>

<style>
  .comp {
    background: var(--s1);
    border-radius: 6px;
    padding: 12px 14px;
    display: grid;
    gap: 10px;
  }
  textarea {
    background: var(--s2);
    border: 0;
    border-radius: 5px;
    color: var(--ink);
    padding: 11px 13px;
    font: 15px var(--sans);
    resize: vertical;
    min-height: 46px;
  }
  textarea::placeholder {
    color: var(--dim);
  }
  .on-item {
    display: flex;
    gap: 10px;
    align-items: baseline;
    background: var(--s2);
    border-radius: 5px;
    padding: 11px 13px;
    min-width: 0;
  }
  .on-item .lbl {
    font: 500 10.5px var(--mono);
    letter-spacing: 0.09em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .on-item .ttl {
    font: 15px var(--sans);
    color: var(--ink);
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .on-item a {
    font: 12.5px var(--sans);
  }
  .line {
    display: flex;
    gap: 12px;
    align-items: center;
  }
  .ctx {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 3px 10px;
    align-items: baseline;
    font: 12px var(--mono);
    color: var(--ink2);
  }
  /* A middot between each fact, drawn rather than typed so the list can wrap. */
  .ctx > span + span::before,
  .ctx > span + .lnk::before {
    content: '· ';
    color: var(--dim);
  }
  .ctx .lnk {
    font: 12px var(--mono);
  }
  .adjust {
    background: var(--s2);
    border-radius: 5px;
    padding: 13px 14px;
    display: grid;
    gap: 13px;
  }
  label,
  .field {
    display: grid;
    gap: 5px;
    min-width: 0;
  }
  label > span,
  .field > span {
    font: 500 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
    display: flex;
    gap: 10px;
    align-items: baseline;
  }
  label > span em {
    color: var(--dim);
    font: 11px var(--mono);
    font-style: normal;
    letter-spacing: 0;
    text-transform: none;
  }
  select,
  input {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
  }
  small {
    font-size: 12.5px;
    color: var(--dim);
  }
  small.crit {
    color: var(--crit);
  }
  .seg {
    display: flex;
    background: var(--s1);
    border-radius: 4px;
    padding: 3px;
    gap: 3px;
    width: max-content;
  }
  .seg button {
    padding: 5px 12px;
    border-radius: 3px;
    font: 500 13px var(--sans);
    color: var(--ink2);
    background: none;
    border: 0;
    cursor: pointer;
  }
  .seg button.on {
    background: var(--s3);
    color: var(--ink);
  }
  .pick {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 16px;
    align-items: center;
  }
  .pick label {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: 2px 7px;
    align-items: center;
    max-width: 320px;
    font: 13px var(--sans);
    color: var(--ink);
    cursor: pointer;
  }
  .pick label small {
    grid-column: 2;
    color: var(--dim);
    font: 11px/1.35 var(--mono);
  }
  .pick label.no {
    color: var(--dim);
    cursor: not-allowed;
  }
  .pick input {
    accent-color: var(--acc);
    margin: 0;
  }
  .banner i a {
    color: inherit;
  }
  /* The phone types here too: 16px keeps iOS from zooming the field. */
  @media (max-width: 700px) {
    textarea,
    select,
    input {
      font-size: 16px;
    }
    .line {
      flex-direction: column;
      align-items: stretch;
    }
    .line .btn {
      padding: 11px 14px;
    }
  }
</style>
