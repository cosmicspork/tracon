<script lang="ts">
  import BriefPanel from '../components/BriefPanel.svelte'
  import ContextPanel from '../components/ContextPanel.svelte'
  import ContinuationPanel from '../components/ContinuationPanel.svelte'
  import CriteriaPanel from '../components/CriteriaPanel.svelte'
  import EvidenceLinks from '../components/EvidenceLinks.svelte'
  import { api } from '../lib/api'
  import { autogrow } from '../lib/autogrow'
  import { clock } from '../lib/clock.svelte'
  import { formatAge, formatBudget } from '../lib/format'
  import { isNotFound } from '../lib/load'
  import { renderMessage } from '../lib/markdown'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'
  import type { Brief, Criteria, Project, Session, WorkView } from '../lib/types'
  import {
    blockersLine,
    pickableProjects,
    priorityLabel,
    projectLabel,
    projectName,
    short,
    workLabel,
    workState,
  } from '../lib/work'

  let { id }: { id: string } = $props()

  let item = $state<WorkView | null>(null)
  let brief = $state<Brief | null>(null)
  let criteria = $state<Criteria | null>(null)
  let sessions = $state<Session[]>([])
  let discovered = $state<{ id: string; title: string; state: string }[]>([])
  let titles = $state<Map<string, string>>(new Map())
  let error = $state<string | null>(null)
  let loaded = $state(false)
  /** The last read failed for a reason other than the item not being there. */
  let failed = $state(false)
  let busy = $state(false)
  let depInput = $state('')
  let projects = $state<Project[]>([])
  let editing = $state(false)
  let confirmingDelete = $state(false)
  let draft = $state({ title: '', body: '', priority: '', project: '' })

  const levels = $derived(store.node?.work?.priority_levels ?? [])

  async function load() {
    try {
      const d = await api.workItem(id)
      failed = false
      item = d.item
      brief = d.brief
      criteria = d.criteria
      sessions = d.sessions
      discovered = d.discovered
      if (d.item) {
        const [all, known] = await Promise.all([
          api.work(d.item.channel),
          api.projects(d.item.channel).catch(() => ({ projects: [] as Project[] })),
        ])
        titles = new Map(all.items.map((i) => [i.id, i.title]))
        projects = known.projects
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      failed = !isNotFound(e)
    } finally {
      loaded = true
    }
  }
  $effect(() => {
    void id
    void store.workVersion
    void load()
  })

  const ws = $derived(item ? workState(item) : 'ready')
  const parent = $derived(item?.discovered_from ? titles.get(item.discovered_from) : undefined)

  async function act(f: () => Promise<unknown>) {
    busy = true
    error = null
    try {
      await f()
      await load()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
  /** Close the task: the item closes, and any session holding it ends. */
  function close() {
    return act(() => api.closeTask(id))
  }
  function reopen() {
    return act(() => api.putWork(id, { state: 'open' }))
  }
  function addDep() {
    const dep = depInput.trim()
    if (!dep || !item) return
    const full = [...titles.keys()].find((k) => k.startsWith(dep)) ?? dep
    return act(async () => {
      await api.putWork(id, { deps: [...item!.deps, full] })
      depInput = ''
    })
  }
  function dropDep(dep: string) {
    return act(() => api.putWork(id, { deps: item!.deps.filter((d) => d !== dep) }))
  }
  function remove() {
    return act(async () => {
      await api.deleteWork(id)
      router.go('/tasks')
    })
  }
  function edit() {
    if (!item) return
    draft = {
      title: item.title,
      body: item.body,
      priority: String(item.priority),
      project: item.project_id ?? '',
    }
    confirmingDelete = false
    editing = true
  }
  const draftPriority = $derived(Number(draft.priority))
  const draftValid = $derived(draft.title.trim() !== '' && draft.priority.trim() !== '' && Number.isInteger(draftPriority))
  function save() {
    if (!item || !draftValid) return
    const patch: Parameters<typeof api.putWork>[1] = {}
    if (draft.title.trim() !== item.title) patch.title = draft.title.trim()
    if (draft.body.trim() !== item.body) patch.body = draft.body
    if (draftPriority !== item.priority) patch.priority = draftPriority
    if ((draft.project || null) !== item.project_id) patch.project_id = draft.project || null
    return act(async () => {
      if (Object.keys(patch).length) await api.putWork(id, patch)
      editing = false
    })
  }
  function phaseLabel(s: Session): string {
    const end = s.end_reason === 'phase_done' ? (s.phase === 'plan' ? 'planned' : 'reviewed') : s.end_reason ?? s.state
    return `${s.phase} ${short(s.id)} · ${end}`
  }
</script>

{#if !loaded}
  <div class="empty">Loading…</div>
{:else if !item && failed}
  <div class="banner crit" role="alert">
    Could not load this task <b>· {error}</b>
    <button class="lnk" onclick={() => { error = null; void load() }}>Retry</button>
  </div>
{:else if !item}
  <div class="banner crit">not found <b>· {error ?? `no task ${id.slice(0, 8)}`}</b></div>
{:else}
  <div class="h4"><a class="lnk" href="/tasks">‹ Tasks</a></div>
  <div class="head {ws}">
    <span class="bar"></span>
    <span class="t">
      <em>{workLabel(item)}</em>
      {item.title}
      <small>{short(item.id, 12)} · {item.channel} · added {formatAge(item.created_ms, clock.now)}</small>
    </span>
  </div>

  <dl class="kv">
    <dt>Priority</dt>
    <dd class="m">{priorityLabel(item.priority, levels)}{levels.length ? '' : ' · higher runs first'}</dd>
    <dt>Project</dt>
    <dd class="m">{projectName(item, projects) ?? 'none'}</dd>
    <dt>Plan</dt>
    <dd class="m">
      {#if item.phase_plan_slug}<a href="/docs/{item.channel}/{item.phase_plan_slug}">{item.phase_plan_slug}</a>{:else}none yet · a plan session writes it{/if}
    </dd>
    <dt>Brief</dt>
    <dd class="m">
      {#if item.brief_slug}<a href="/docs/{item.channel}/{item.brief_slug}">{item.brief_slug}</a>{:else}none · optional{/if}
    </dd>
    <dt>Waits on</dt>
    <dd class="m">
      {#if item.deps.length === 0}nothing{:else}
        {#each item.deps as d (d)}
          <span class="dep"><a href="/tasks/{d}">{short(d)}</a>{titles.has(d) ? ` ${titles.get(d)}` : ''}{#if !surface.phone && item.state === 'open'}<button class="lnk d" onclick={() => dropDep(d)} disabled={busy}>×</button>{/if}</span>
        {/each}
      {/if}
      {#if item.readiness.state === 'blocked'}<span class="why"> · {blockersLine(item.readiness.by, titles)}</span>{/if}
    </dd>
    {#if item.discovered_from}
      <dt>Discovered from</dt>
      <dd class="m"><a href="/tasks/{item.discovered_from}">{short(item.discovered_from)}</a>{parent ? ` ${parent}` : ''}{#if item.discovered_by_session}{' · by session '}<a href="/sessions/{item.discovered_by_session}">{short(item.discovered_by_session)}</a>{/if}{#if item.discovered_by_lane}{` · by lane ${item.discovered_by_lane}`}{/if}</dd>
    {:else if item.discovered_by_lane}
      <dt>Discovered by</dt>
      <dd class="m">lane {item.discovered_by_lane} · a harness you run yourself</dd>
    {/if}
    <dt>Sessions</dt>
    <dd class="m">
      {#if sessions.length === 0}none yet{:else}
        {#each sessions as s (s.id)}
          <span class="dep"><a href="/sessions/{s.id}">{phaseLabel(s)}</a> · {formatBudget(s.tokens_used, s.budget_tokens)}</span>
        {/each}
      {/if}
    </dd>
    {#if item.closed_by_session}
      <dt>Closed</dt>
      <dd class="m">by session <a href="/sessions/{item.closed_by_session}">{short(item.closed_by_session)}</a> · {formatAge(item.updated_ms, clock.now)} ago</dd>
    {/if}
  </dl>

  {#if editing}
    <form class="edit" onsubmit={(e) => (e.preventDefault(), save())}>
      <label class="field">
        <span>Title</span>
        <input bind:value={draft.title} />
      </label>
      <label class="field">
        <span>Requirements</span>
        <textarea class="grow" bind:value={draft.body} use:autogrow={draft.body} spellcheck="true"></textarea>
      </label>
      <div class="row2">
        <label class="pick">
          <span>Priority</span>
          {#if levels.length}
            <select bind:value={draft.priority}>
              {#each levels as l (l.value)}<option value={String(l.value)}>{l.name}</option>{/each}
              {#if !levels.some((l) => String(l.value) === draft.priority)}<option value={draft.priority}>{draft.priority}</option>{/if}
            </select>
          {:else}
            <input class="num" bind:value={draft.priority} inputmode="numeric" pattern="-?[0-9]*" />
          {/if}
        </label>
        <label class="pick">
          <span>Project</span>
          <select bind:value={draft.project}>
            <option value="">No project</option>
            {#each pickableProjects(projects, item.project_id) as p (p.id)}<option value={p.id}>{projectLabel(p, projects)}</option>{/each}
            {#if draft.project && !projects.some((p) => p.id === draft.project)}<option value={draft.project}>{short(draft.project)}</option>{/if}
          </select>
        </label>
        <button class="btn p" type="submit" disabled={busy || !draftValid}>Save</button>
        <button class="lnk" type="button" onclick={() => (editing = false)} disabled={busy}>Cancel</button>
      </div>
    </form>
  {:else if item.body.trim()}
    <div class="body md">{@html renderMessage(item.body)}</div>
  {/if}

  <ContinuationPanel itemId={item.id} showIntent={false} onchange={load} />

  <BriefPanel {item} {brief} onchange={load} />
  <CriteriaPanel {item} {criteria} onchange={load} />
  <ContextPanel {item} />

  <!-- One list for the item: the node's work item filter covers every
       candidate its sessions captured, and each card names its session. -->
  <EvidenceLinks workItemId={item.id} channel={item.channel} />

  {#if discovered.length}
    <div class="h5">Discovered from this task <b>{discovered.length}</b></div>
    <div class="chain">
      {#each discovered as d (d.id)}
        <span>{short(item.id)}</span><span class="arr">→</span><a href="/tasks/{d.id}">{short(d.id)} {d.title}</a><span class="chip" class:ok={d.state === 'closed'}>{d.state}</span>
      {/each}
    </div>
  {/if}

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
  {/if}

  {#if !surface.phone}
    {#if item.state === 'open'}
      <div class="deps">
        <input placeholder="add a dependency by id prefix" bind:value={depInput} onkeydown={(e) => e.key === 'Enter' && addDep()} />
        <button class="btn" onclick={addDep} disabled={busy || !depInput.trim()}>Waits on</button>
      </div>
      <div class="actions">
        {#if ws === 'ready'}
          <a class="btn" class:p={!item.phase_plan_slug} href="/?item={item.id}&phase=plan">{item.phase_plan_slug ? 'Plan again' : 'Plan'}</a>
          {#if item.phase_plan_slug}
            <a class="btn p" href="/?item={item.id}&phase=execute">Execute</a>
          {:else}
            <span class="btn" aria-disabled="true" title="needs a plan">Execute · needs a plan</span>
          {/if}
        {:else if ws === 'insession' && item.session_id}
          <a class="btn p" href="/sessions/{item.session_id}">Open session</a>
        {/if}
        {#if !editing}<button class="lnk" onclick={edit} disabled={busy}>Edit</button>{/if}
        <button class="lnk d" onclick={close} disabled={busy}>Close task</button>
        {#if confirmingDelete}
          <span class="confirm">Delete this task for good? <button class="btn d" onclick={remove} disabled={busy}>Delete</button>
            <button class="lnk" onclick={() => (confirmingDelete = false)} disabled={busy}>Keep it</button></span
          >
        {:else}
          <button class="lnk d" onclick={() => (confirmingDelete = true)} disabled={busy}>Delete</button>
        {/if}
      </div>
    {:else}
      <div class="actions">
        <button class="lnk" onclick={reopen} disabled={busy}>Reopen</button>
        {#if !editing}<button class="lnk" onclick={edit} disabled={busy}>Edit</button>{/if}
      </div>
    {/if}
  {/if}
{/if}

<style>
  .edit {
    display: grid;
    gap: 8px;
    margin: 12px 0;
    max-width: 820px;
  }
  .edit .field {
    display: grid;
    gap: 3px;
  }
  .edit .field > span,
  .edit .pick > span {
    color: var(--ink2);
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
  .edit input,
  .edit select,
  .edit textarea {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
  }
  /* The title above it and the priority row and buttons below it. */
  .edit textarea {
    --chrome: 260px;
    font: 13px/1.5 var(--mono);
    box-sizing: border-box;
  }
  .edit .row2 {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    align-items: center;
  }
  .edit .pick {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .edit .num {
    width: 56px;
  }
  .confirm {
    display: inline-flex;
    gap: 8px;
    align-items: center;
    color: var(--crit);
  }
  .head {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr);
    gap: 0 14px;
    align-items: center;
    background: linear-gradient(90deg, var(--wash-run), var(--s1) 42%);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
  }
  .head .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--acc);
  }
  .head.blocked {
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 42%);
  }
  .head.blocked .bar {
    background: var(--dim);
  }
  .head.insession {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  .head.insession .bar {
    background: var(--wait);
  }
  .head.closed {
    background: linear-gradient(90deg, var(--wash-ok), var(--s1) 42%);
  }
  .head.closed .bar {
    background: var(--ok);
  }
  .t {
    font-weight: 500;
    min-width: 0;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--acc);
  }
  .head.blocked .t em {
    color: var(--dim);
  }
  .head.insession .t em {
    color: var(--wait);
  }
  .head.closed .t em {
    color: var(--ok);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    margin-top: 2px;
  }
  .kv {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 5px 16px;
    font-size: 13.5px;
    margin: 0;
  }
  .kv dt {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    padding-top: 3px;
  }
  .kv dd {
    margin: 0;
  }
  .kv dd.m {
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .dep {
    display: block;
  }
  .dep .lnk {
    margin-left: 6px;
    font-size: 12px;
  }
  .why {
    color: var(--dim);
  }
  .body {
    background: var(--s1);
    border-radius: 4px;
    padding: 12px 14px;
    max-width: 70ch;
    overflow-wrap: anywhere;
  }
  .md > :global(:first-child) {
    margin-top: 0;
  }
  .md > :global(:last-child) {
    margin-bottom: 0;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol),
  .md :global(pre),
  .md :global(blockquote) {
    margin: 4px 0;
  }
  .md :global(ul),
  .md :global(ol) {
    padding-left: 2.5ch;
  }
  .md :global(h1),
  .md :global(h2),
  .md :global(h3),
  .md :global(h4) {
    margin: 0.7em 0 0.2em;
    font-size: 1em;
    font-weight: 600;
    color: var(--ink);
  }
  .md :global(code) {
    font: 0.9em var(--mono);
    background: var(--s2);
    border-radius: 3px;
    padding: 0 3px;
  }
  .md :global(pre) {
    background: var(--s2);
    border-radius: 4px;
    padding: 8px 10px;
    overflow-x: auto;
  }
  .md :global(pre code) {
    background: none;
    padding: 0;
    overflow-wrap: normal;
  }
  .md :global(blockquote) {
    border-left: 2px solid var(--rule);
    padding-left: 1.5ch;
  }
  .md :global(a) {
    color: var(--acc);
  }
  .md :global(.no-fetch) {
    color: var(--dim);
  }
  .h5 {
    font: 11.5px var(--mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--dim);
    margin: 8px 0 0;
  }
  .h5 b {
    color: var(--ink2);
    font-weight: 400;
    margin-left: 6px;
  }
  .chain {
    display: flex;
    flex-wrap: wrap;
    gap: 6px 10px;
    align-items: center;
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .chain .arr {
    color: var(--dim);
  }
  .chip.ok {
    background: var(--wash-ok);
    color: var(--ok);
  }
  .deps {
    display: flex;
    gap: 8px;
    max-width: 520px;
  }
  .deps input {
    flex: 1;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--sans);
  }
  .actions {
    display: flex;
    gap: 12px;
    align-items: center;
    flex-wrap: wrap;
  }
  .actions a.btn {
    text-decoration: none;
  }
  .actions span.btn {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
