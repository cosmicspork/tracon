<script lang="ts">
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge, formatBudget } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'
  import type { Session, WorkView } from '../lib/types'
  import { blockersLine, short, workLabel, workState } from '../lib/work'

  let { id }: { id: string } = $props()

  let item = $state<WorkView | null>(null)
  let sessions = $state<Session[]>([])
  let discovered = $state<{ id: string; title: string; state: string }[]>([])
  let titles = $state<Map<string, string>>(new Map())
  let error = $state<string | null>(null)
  let loaded = $state(false)
  let busy = $state(false)
  let depInput = $state('')

  async function load() {
    try {
      const d = await api.workItem(id)
      item = d.item
      sessions = d.sessions
      discovered = d.discovered
      if (d.item) {
        const all = await api.work(d.item.channel)
        titles = new Map(all.items.map((i) => [i.id, i.title]))
      }
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
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
  function close() {
    return act(() => api.putWork(id, { state: 'closed' }))
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
      router.go('/work')
    })
  }
  function phaseLabel(s: Session): string {
    const end = s.end_reason === 'phase_done' ? (s.phase === 'plan' ? 'planned' : 'reviewed') : s.end_reason ?? s.state
    return `${s.phase} ${short(s.id, 13)} · ${end}`
  }
</script>

{#if !loaded}
  <div class="empty">Loading…</div>
{:else if !item}
  <div class="banner crit">not found <b>· {error ?? `no work item ${id.slice(0, 8)}`}</b></div>
{:else}
  <div class="h4"><a class="lnk" href="/work">‹ Work</a></div>
  <div class="head {ws}">
    <span class="bar"></span>
    <span class="t">
      <em>{workLabel(item)}</em>
      {item.title}
      <small>{short(item.id, 12)} · {item.channel}{item.project_id ? ` · project ${short(item.project_id)}` : ''} · p{item.priority} · added {formatAge(item.created_ms, clock.now)}</small>
    </span>
  </div>

  <dl class="kv">
    <dt>Plan</dt>
    <dd class="m">
      {#if item.phase_plan_slug}<a href="/docs/{item.channel}/{item.phase_plan_slug}">{item.phase_plan_slug}</a>{:else}none yet · a plan session writes it{/if}
    </dd>
    <dt>Waits on</dt>
    <dd class="m">
      {#if item.deps.length === 0}nothing{:else}
        {#each item.deps as d (d)}
          <span class="dep"><a href="/work/{d}">{short(d)}</a>{titles.has(d) ? ` ${titles.get(d)}` : ''}{#if !surface.phone && item.state === 'open'}<button class="lnk d" onclick={() => dropDep(d)} disabled={busy}>×</button>{/if}</span>
        {/each}
      {/if}
      {#if item.readiness.state === 'blocked'}<span class="why"> · {blockersLine(item.readiness.by, titles)}</span>{/if}
    </dd>
    {#if item.discovered_from}
      <dt>Discovered from</dt>
      <dd class="m"><a href="/work/{item.discovered_from}">{short(item.discovered_from)}</a>{parent ? ` ${parent}` : ''}{item.discovered_by_session ? ` · by session ${item.discovered_by_session.slice(-6)}` : ''}</dd>
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
      <dd class="m">by session <a href="/sessions/{item.closed_by_session}">{item.closed_by_session.slice(-6)}</a> · {formatAge(item.updated_ms, clock.now)} ago</dd>
    {/if}
  </dl>

  {#if item.body.trim()}
    <div class="body">{item.body}</div>
  {/if}

  {#if discovered.length}
    <div class="h5">Discovered from this item <b>{discovered.length}</b></div>
    <div class="chain">
      {#each discovered as d (d.id)}
        <span>{short(item.id)}</span><span class="arr">→</span><a href="/work/{d.id}">{short(d.id)} {d.title}</a><span class="chip" class:ok={d.state === 'closed'}>{d.state}</span>
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
          <a class="btn" class:p={!item.phase_plan_slug} href="/new?item={item.id}&phase=plan">{item.phase_plan_slug ? 'Plan again' : 'Plan'}</a>
          {#if item.phase_plan_slug}
            <a class="btn p" href="/new?item={item.id}&phase=execute">Execute</a>
          {:else}
            <span class="btn" aria-disabled="true" title="needs a plan">Execute · needs a plan</span>
          {/if}
        {:else if ws === 'insession' && item.session_id}
          <a class="btn p" href="/sessions/{item.session_id}">Open session</a>
        {/if}
        <button class="lnk d" onclick={close} disabled={busy}>Close</button>
        <button class="lnk d" onclick={remove} disabled={busy}>Delete</button>
      </div>
    {:else}
      <div class="actions"><button class="lnk" onclick={reopen} disabled={busy}>Reopen</button></div>
    {/if}
  {/if}
{/if}

<style>
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
    white-space: pre-wrap;
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
