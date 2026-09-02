<script lang="ts">
  // Starting work is typing what needs doing. The channel decides the rest —
  // which model plans, which builds, what the budget is — so the fields that
  // used to be a form are a disclosure most starts never open.
  //
  // Two modes. Given an item, it starts a session on that item and the prompt
  // is replaced by the item's title. Given none, the prompt writes the item.
  import RepoPicker from './RepoPicker.svelte'
  import { api, ApiError } from '../lib/api'
  import { modelLabel, phaseDefaults } from '../lib/bindings'
  import { formatTokens } from '../lib/format'
  import { eligibleNodes } from '../lib/nodes'
  import { repoLabel } from '../lib/repo'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { WorkView } from '../lib/types'

  let { item = null, phase = $bindable('plan') }: { item?: WorkView | null; phase?: 'plan' | 'execute' } =
    $props()

  let prompt = $state('')
  let channel = $state('')
  let repo = $state('')
  let branch = $state('')
  let model = $state('')
  let budget = $state('')
  let nodeId = $state<string | null>(null)
  let open = $state(false)
  let busy = $state(false)
  let error = $state<string | null>(null)
  let savedItem = $state<string | null>(null)
  let touched = $state(false)

  // An archived channel takes no new sessions, so it is not offered.
  const channelNames = $derived(store.channels.filter((c) => !c.archived).map((c) => c.name))
  const memberships = $derived(Object.fromEntries(store.channels.map((c) => [c.name, c.nodes])))
  const eligible = $derived(eligibleNodes(store.nodes, memberships, channel))
  const node = $derived(
    eligible.find((n) => n.id === nodeId) ?? eligible.find((n) => n.is_self) ?? eligible[0] ?? store.node,
  )
  const channelInfo = $derived(store.channels.find((c) => c.name === channel))
  const atCeiling = $derived(channelInfo?.ceiling.state === 'at')
  const blocked = $derived(!node || node.state === 'refused' || node.harness.mismatch === true || !node.reachable)
  const bound = $derived(phaseDefaults(channelInfo?.bindings, phase))
  const models = $derived(node?.models ?? [])
  // What the context line promises: the model each phase will use.
  const planLabel = $derived(modelLabel(phaseDefaults(channelInfo?.bindings, 'plan').model, models))
  const execLabel = $derived(modelLabel(phaseDefaults(channelInfo?.bindings, 'execute').model, models))
  const needsPlan = $derived(phase === 'execute' && item !== null && !item.phase_plan_slug)
  const ready = $derived(
    !blocked &&
      !atCeiling &&
      !needsPlan &&
      channel !== '' &&
      repo.trim() !== '' &&
      model !== '' &&
      (item !== null || prompt.trim() !== '') &&
      !busy,
  )

  // The channel the node actually has; the item's own channel when there is one.
  $effect(() => {
    if (item) {
      channel = item.channel
      return
    }
    if (!channel && channelNames.length) {
      channel = channelNames.includes('personal') ? 'personal' : channelNames[0]
    }
  })
  // The repository this channel worked in last, so the common case needs no pick.
  $effect(() => {
    if (repo !== '' || !channel) return
    const last = [...store.sessions.values()]
      .sort((a, b) => b.created_ms - a.created_ms)
      .find((s) => s.channel === channel && s.repo_path)
    if (last) repo = last.repo_path
  })
  $effect(() => {
    if (touched) return
    if (bound.model && models.some((m) => m.value === bound.model)) {
      model = bound.model
      return
    }
    if (model !== '' || models.length === 0) return
    const last = [...store.sessions.values()]
      .sort((a, b) => b.created_ms - a.created_ms)
      .find((s) => models.some((m) => m.value === s.model))
    if (last) model = last.model
  })
  $effect(() => {
    budget = bound.budget_tokens ? String(bound.budget_tokens) : budget || '2000000'
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
        repo_path: repo.trim(),
        branch: branch.trim() || undefined,
        phase,
        model,
        budget_tokens: Number(budget) || undefined,
        node_id: node && !node.is_self ? node.id : undefined,
      }
      // The first line names the item; the rest is what done looks like.
      const lines = prompt.trim().split('\n')
      const session = item
        ? await api.createSession({ ...common, work_item_id: item.id })
        : (await api.compose({ ...common, title: lines[0], body: lines.slice(1).join('\n').trim() })).session
      prompt = ''
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
      placeholder="What should get done?"
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
      {#if busy}Starting…{:else if atCeiling}{channel} is at its ceiling{:else}Start {phase}{/if}
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
      <div class="field">
        <span>Repository</span>
        <RepoPicker bind:value={repo} {channel} />
      </div>
      <label>
        <span>Branch</span>
        <input bind:value={branch} placeholder="feat/…  (a name is generated if empty)" spellcheck="false" />
        <small>The worktree is created from origin's default branch, outside the repo.</small>
      </label>
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
      <label>
        <span>Model <em>{bound.model ? `${channel} binds one to ${phase}` : 'this session only'}</em></span>
        <select bind:value={model} onchange={() => (touched = true)}>
          <option value="" disabled>Choose a model</option>
          {#each models as m (m.value)}<option value={m.value}>{m.name}</option>{/each}
        </select>
        {#if models.length === 0}
          <small class="crit">The node offered no models; connect a provider on the Nodes screen.</small>
        {/if}
      </label>
      <label>
        <span>Budget <em>tokens</em></span>
        <input bind:value={budget} inputmode="numeric" pattern="[0-9]*" />
        <small>The session is killed at this number, checked at each turn's end.</small>
      </label>
      <div class="field">
        <span>Runs on</span>
        {#if store.nodes.length > 1}
          <div class="pick">
            {#each store.nodes as n (n.id)}
              {@const ok = eligible.some((e) => e.id === n.id)}
              <label class:no={!ok}>
                <input
                  type="radio"
                  name="node"
                  value={n.id}
                  disabled={!ok}
                  checked={node?.id === n.id}
                  onchange={() => (nodeId = n.id)}
                />
                <span class="chip" class:bad={n.state === 'refused' || n.harness.mismatch} class:off={!n.reachable}
                  >{n.name}</span
                >
              </label>
            {/each}
          </div>
        {:else if node?.state === 'refused'}
          <span class="chip bad">{node.name}</span>
          <small class="crit">Refused: {node.failed_check}: {node.failed_detail}</small>
        {:else if node?.harness.mismatch}
          <span class="chip bad">{node.name}</span>
          <small class="crit">Version mismatch: node expects {node.harness.pinned}, host has {node.harness.found}.</small>
        {:else if node && !node.reachable}
          <span class="chip off">{node.name}</span>
          <small class="crit">Unreachable. Start when it returns.</small>
        {:else if node}
          <span class="chip">{node.name}</span>
          <small>{node.harness.id} {node.harness.found ?? node.harness.pinned} · boundary check passed</small>
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
    display: inline-flex;
    gap: 7px;
    align-items: center;
    font: 13px var(--sans);
    color: var(--ink);
    cursor: pointer;
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
