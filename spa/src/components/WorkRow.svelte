<script lang="ts">
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { nodeLabel } from '../lib/nodes'
  import { store } from '../lib/store.svelte'
  import type { WorkView } from '../lib/types'
  import { blockersLine, short, workLabel, workState } from '../lib/work'

  let { item, titles }: { item: WorkView; titles: Map<string, string> } = $props()

  const state = $derived(workState(item))
  const holder = $derived(item.session_id ? store.sessions.get(item.session_id) : undefined)
  const detail = $derived.by(() => {
    const bits = [short(item.id)]
    if (state === 'blocked' && item.readiness.state === 'blocked') bits.push(blockersLine(item.readiness.by, titles))
    if (state === 'ready') bits.push(item.phase_plan_slug ? 'plan written' : 'no plan yet')
    if (state === 'insession' && holder) bits.push(`${holder.phase} on ${nodeLabel(store.nodes, holder.node_id)}`)
    if (state === 'closed' && item.closed_by_session) bits.push(`closed by session ${item.closed_by_session.slice(-6)}`)
    if (item.discovered_from) bits.push(`discovered from ${short(item.discovered_from)}`)
    return bits.join(' · ')
  })
  const act = $derived(
    state === 'ready' ? (item.phase_plan_slug ? 'Plan · Execute' : 'Plan') : state === 'insession' ? 'Session' : 'Open',
  )
</script>

<a class="row {state}" href={state === 'insession' && item.session_id ? `/sessions/${item.session_id}` : `/work/${item.id}`}>
  <span class="bar"></span>
  <span class="mono pri" class:hi={item.priority >= 5}>p{item.priority} · {formatAge(item.created_ms, clock.now)}</span>
  <span class="t">
    <em>{workLabel(item)}</em>
    {item.title}
    <small>{detail}</small>
  </span>
  <span class="act">{act}</span>
</a>

<style>
  .row {
    display: grid;
    grid-template-columns: 3px 72px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
    text-decoration: none;
    color: inherit;
  }
  .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--acc);
  }
  .row.ready {
    background: linear-gradient(90deg, var(--wash-run), var(--s1) 42%);
  }
  .row.ready .t em {
    color: var(--acc);
  }
  .row.blocked .bar {
    background: var(--dim);
  }
  .row.blocked {
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 42%);
  }
  .row.blocked .t em,
  .row.blocked .act {
    color: var(--dim);
  }
  .row.insession .bar {
    background: var(--wait);
  }
  .row.insession {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  .row.insession .t em {
    color: var(--wait);
  }
  .row.closed .bar {
    background: var(--ok);
  }
  .row.closed {
    background: linear-gradient(90deg, var(--wash-ok), var(--s1) 42%);
  }
  .row.closed .t em {
    color: var(--ok);
  }
  .pri {
    color: var(--dim);
  }
  .pri.hi {
    color: var(--wait);
  }
  .t {
    font-weight: 500;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--ink2);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    margin-top: 2px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .act {
    color: var(--acc);
    font-weight: 500;
    white-space: nowrap;
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: 3px minmax(0, 1fr);
    }
    .row > .mono,
    .act {
      display: none;
    }
  }
</style>
