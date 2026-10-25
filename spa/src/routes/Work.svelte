<script lang="ts">
  import LoadGate from '../components/LoadGate.svelte'
  import WorkRow from '../components/WorkRow.svelte'
  import { api } from '../lib/api'
  import { defaultChannel, rememberChannel, rememberedChannel } from '../lib/channel'
  import { hasAnswer } from '../lib/load'
  import { Loader } from '../lib/loader.svelte'
  import { router } from '../lib/router.svelte'
  import { tasksSearch, tasksView } from '../lib/routes'
  import { store } from '../lib/store.svelte'
  import type { Project, WorkView } from '../lib/types'
  import {
    inProject,
    matches,
    projectName,
    pickableProjects,
    projectLabel,
    projectTabs,
    rememberedProject,
    rememberProject,
    workState,
  } from '../lib/work'

  let channel = $state<string>('')
  const work = new Loader<WorkView[]>()
  let projects = $state<Project[]>([])
  let error = $state<string | null>(null)
  let adding = $state(false)
  let title = $state('')
  let body = $state('')
  let newProject = $state('')
  let busy = $state(false)
  let showClosed = $state(false)

  const levels = $derived(store.node?.work?.priority_levels ?? [])
  const defaultPriority = $derived(store.node?.work?.default_priority ?? 0)
  let priority = $state<string | null>(null)

  const channels = $derived(store.channels.filter((c) => !c.archived).map((c) => c.name))
  $effect(() => {
    if (!channel && channels.length)
      channel = defaultChannel({
        names: channels,
        remembered: rememberedChannel(),
        nodeDefault: store.node?.default_channel,
      })
  })
  $effect(() => {
    void store.workVersion
    void store.sessions
    if (!channel) return
    const name = channel
    work.load(name, () => api.work(name).then((d) => d.items))
    api
      .projects(name)
      .then((d) => {
        if (channel === name) projects = d.projects
      })
      .catch(() => {
        // Names are a nicety: without them a project is shown by its id.
      })
  })
  // No channel yet is the channel list still loading; once it has answered
  // with none, there is simply no work.
  const status = $derived(channel ? work : store.loads.channels)
  const shown = $derived(hasAnswer(status))
  const retry = () => (channel ? work.retry() : void store.refetch())

  const items = $derived(work.value ?? [])
  const view = $derived(tasksView(router.search))
  const tabs = $derived(projectTabs(items, projects, view.project))
  const pickable = $derived(pickableProjects(projects))
  // Only worth tabs once some item belongs to a project.
  const tabbed = $derived(tabs.some((t) => t.key !== 'all' && t.key !== 'none'))
  const tab = $derived(tabs.some((t) => t.key === view.project) ? view.project : 'all')

  // Back where the operator left this channel, unless the address says otherwise.
  let restored = ''
  $effect(() => {
    if (!channel || !shown || restored === channel) return
    restored = channel
    if (view.project !== 'all') return
    const kept = rememberedProject(channel)
    if (kept && tabs.some((t) => t.key === kept)) show({ ...view, project: kept }, true)
  })

  function show(next: { project: string; q: string }, replace = false) {
    const path = `/tasks${tasksSearch(next)}`
    if (replace) router.replace(path)
    else router.go(path)
  }

  function pick(key: string) {
    rememberProject(channel, key)
    show({ ...view, project: key })
  }

  const visible = $derived(items.filter((i) => inProject(i, tab) && matches(i, view.q)))
  const filtering = $derived(tab !== 'all' || view.q.trim() !== '')
  const titles = $derived(new Map(items.map((i) => [i.id, i.title])))
  const ready = $derived(visible.filter((i) => workState(i) === 'ready'))
  const blocked = $derived(visible.filter((i) => workState(i) === 'blocked'))
  const inSession = $derived(visible.filter((i) => workState(i) === 'insession'))
  const closed = $derived(visible.filter((i) => workState(i) === 'closed'))
  const openCount = $derived(items.filter((i) => workState(i) !== 'closed').length)
  const closedCount = $derived(items.length - openCount)
  // On All, a row says its project only when there is more than one to tell apart.
  const mixed = $derived(tabs.filter((t) => t.key !== 'all').length > 1)
  const label = (v: WorkView) => (tab === 'all' && mixed ? projectName(v, projects) : null)

  function openForm() {
    adding = true
    newProject = tab !== 'all' && tab !== 'none' && pickable.some((p) => p.id === tab) ? tab : ''
    priority = null
  }

  async function add(e: SubmitEvent) {
    e.preventDefault()
    if (!title.trim()) return
    busy = true
    error = null
    try {
      const chosen = priority === null || priority.trim() === '' ? undefined : Number(priority)
      const item = await api.addWork({
        channel,
        title: title.trim(),
        body: body.trim(),
        ...(chosen === undefined || Number.isNaN(chosen) ? {} : { priority: chosen }),
        ...(newProject ? { project_id: newProject } : {}),
      })
      title = ''
      body = ''
      adding = false
      router.go(`/tasks/${item.id}`)
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

<div class="h4">
  Tasks
  <b>{shown && channel ? `${openCount} open on ${channel}${closedCount ? ` · ${closedCount} closed` : ''}` : `on ${channel || '…'}`}</b>
  <button class="btn p r" type="button" aria-expanded={adding} onclick={() => (adding ? (adding = false) : openForm())}>{adding ? 'Close form' : 'New task'}</button>
</div>

{#if channels.length > 1}
  <div class="bar">
    <label class="channel">
      <span>Channel</span>
      <select bind:value={channel} onchange={() => rememberChannel(channel)}>
        {#each channels as c (c)}<option value={c}>{c}</option>{/each}
      </select>
    </label>
  </div>
{/if}

{#if adding}
  <form class="new" onsubmit={add}>
    <label class="field">
      <span>Title</span>
      <input placeholder="What needs doing" bind:value={title} />
    </label>
    <label class="field">
      <span>Requirements</span>
      <textarea placeholder="What done looks like and what it must not touch" bind:value={body}></textarea>
    </label>
    <div class="row2">
      <label class="pick">
        <span>Priority</span>
        {#if levels.length}
          <select class="pri" value={priority ?? String(defaultPriority)} onchange={(e) => (priority = e.currentTarget.value)}>
            {#each levels as l (l.value)}<option value={String(l.value)}>{l.name}</option>{/each}
          </select>
        {:else}
          <input class="pri num" value={priority ?? String(defaultPriority)} oninput={(e) => (priority = e.currentTarget.value)} inputmode="numeric" pattern="-?[0-9]*" />
        {/if}
      </label>
      {#if pickable.length}
        <label class="pick">
          <span>Project</span>
          <select class="proj" bind:value={newProject}>
            <option value="">No project</option>
            {#each pickable as p (p.id)}<option value={p.id}>{projectLabel(p, projects)}</option>{/each}
          </select>
        </label>
      {/if}
      <button class="btn p" type="submit" disabled={busy || !title.trim()}>Add task</button>
      <small>Dependencies are set on the task afterwards.</small>
    </div>
  </form>
{/if}

{#if error}
  <div class="banner crit">Could not add work <b>· {error}</b></div>
{/if}

<LoadGate {status} what="work" onretry={retry} retrying={channel ? work.pending : store.refetching}>
  {#if items.length}
    <div class="filters">
      {#if tabbed}
        {#if tabs.length > 7}
          <label class="projsel">
            <span>Project</span>
            <select value={tab} onchange={(e) => pick(e.currentTarget.value)}>
              {#each tabs as t (t.key)}<option value={t.key}>{t.label} · {t.open}</option>{/each}
            </select>
          </label>
        {:else}
          <div class="tabs" role="tablist" aria-label="Project">
            {#each tabs as t (t.key)}
              <button role="tab" type="button" class:on={tab === t.key} aria-selected={tab === t.key} onclick={() => pick(t.key)}>
                {t.label} <b>{t.open}</b>
              </button>
            {/each}
          </div>
        {/if}
      {/if}
      <input
        class="search"
        type="search"
        placeholder="Search tasks"
        aria-label="Search tasks"
        value={view.q}
        oninput={(e) => show({ ...view, project: tab, q: e.currentTarget.value }, true)}
      />
    </div>
  {/if}

  <div class="h5">Ready <b>{ready.length}</b></div>
  {#if ready.length === 0}
    {#if filtering && visible.length === 0}
      <div class="empty">
        Nothing here matches. <button class="lnk" type="button" onclick={() => show({ project: 'all', q: '' })}>Show every task</button>
      </div>
    {:else if items.length === closedCount}
      <div class="empty">No open work on {channel || 'this channel'} yet. <button class="lnk" type="button" onclick={openForm}>Add a task</button> to give a session a clear outcome.</div>
    {:else if blocked.length}
      <div class="empty">Every open task here is blocked. Review each blocker below or <button class="lnk" type="button" onclick={openForm}>add unblocked work</button>.</div>
    {:else if inSession.length}
      <div class="empty">An open session is already working. Follow it below or <button class="lnk" type="button" onclick={openForm}>add another task</button>.</div>
    {:else}
      <div class="empty">Nothing open here.</div>
    {/if}
  {:else}
    <div class="rows">
      {#each ready as v (v.id)}<WorkRow item={v} {titles} {levels} project={label(v)} />{/each}
    </div>
  {/if}

  {#if blocked.length}
    <div class="h5">Blocked <b>{blocked.length}</b></div>
    <div class="rows">
      {#each blocked as v (v.id)}<WorkRow item={v} {titles} {levels} project={label(v)} />{/each}
    </div>
  {/if}

  {#if inSession.length}
    <div class="h5">In session <b>{inSession.length}</b></div>
    <div class="rows">
      {#each inSession as v (v.id)}<WorkRow item={v} {titles} {levels} project={label(v)} />{/each}
    </div>
  {/if}

  {#if closed.length}
    <div class="h5">
      Closed <b>{closed.length}</b>
      <button class="lnk" onclick={() => (showClosed = !showClosed)}>{showClosed ? 'Hide' : 'Show'}</button>
    </div>
    {#if showClosed}
      <div class="rows">
        {#each closed as v (v.id)}<WorkRow item={v} {titles} {levels} project={label(v)} />{/each}
      </div>
    {/if}
  {/if}
</LoadGate>

<style>
  .h4 .r {
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
  }
  .h5 {
    font: 600 14px var(--sans);
    color: var(--ink2);
    margin: 14px 0 6px;
    display: flex;
    gap: 10px;
    align-items: baseline;
  }
  .h5 b {
    color: var(--ink2);
    font-weight: 400;
  }
  .h5 .lnk {
    letter-spacing: 0;
    text-transform: none;
    font-size: 12px;
  }
  .bar select,
  .new input,
  .new textarea,
  .new select,
  .filters select,
  .search {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
  }
  .bar .channel {
    display: grid;
    gap: 3px;
  }
  .bar .channel span,
  .field > span,
  .pick > span,
  .projsel > span {
    color: var(--ink2);
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
  .new {
    display: grid;
    gap: 8px;
    max-width: 640px;
  }
  .field {
    display: grid;
    gap: 3px;
  }
  .new textarea {
    min-height: 90px;
    resize: vertical;
  }
  .row2 {
    display: flex;
    gap: 12px;
    align-items: center;
  }
  .pick,
  .projsel {
    display: flex;
    gap: 8px;
    align-items: center;
  }
  .pri.num {
    width: 56px;
  }
  .filters {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 12px;
    align-items: center;
    margin: 6px 0 2px;
  }
  .tabs {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .tabs button {
    font: 12px var(--mono);
    padding: 4px 10px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--s1);
    color: var(--dim);
    cursor: pointer;
  }
  .tabs button b {
    font-weight: 400;
    margin-left: 4px;
  }
  .tabs button.on {
    color: var(--ink);
    border-color: currentColor;
  }
  .search {
    flex: 1;
    min-width: 180px;
    max-width: 320px;
    margin-left: auto;
  }
  .row2 .btn {
    white-space: nowrap;
  }
  .row2 small {
    color: var(--dim);
    font-size: 12px;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  /* Adding an item is directing work, so the phone gets it too: full-width
     fields, 16px inputs so iOS does not zoom the form. */
  @media (max-width: 700px) {
    .h4 {
      align-items: center;
      flex-wrap: wrap;
    }
    .h4 .r {
      margin-left: 0;
    }
    .new input,
    .new textarea,
    .new select,
    .filters select,
    .search {
      font-size: 16px;
    }
    .search {
      max-width: none;
      margin-left: 0;
    }
    .row2 {
      flex-wrap: wrap;
    }
    .row2 .btn {
      flex: 1;
    }
  }
</style>
