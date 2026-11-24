<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import {
    laneKey,
    liveness,
    loadDismissals,
    prune,
    saveDismissals,
    visibleLanes,
    type Dismissals,
  } from '../lib/externalLanes'
  import { formatAge } from '../lib/format'
  import type { ExternalEvent, ExternalLane, ExternalView } from '../lib/types'
  import { codeSpans } from '../lib/markdown'
  import { Row, SideButton } from '@tracon/ui/svelte'

  // Harnesses the operator runs themselves have no session: what they did is
  // grouped by the label each gives, for the last day, plus anything still
  // waiting on the operator. Read on an interval; a lane is a summary, and a
  // few seconds behind is fine for one.
  let view = $state<ExternalView | null>(null)
  let dismissed = $state<Dismissals>(loadDismissals())
  let open = $state<string | null>(null)
  let log = $state<ExternalEvent[]>([])
  let error = $state<string | null>(null)
  let starting = $state('')

  async function refresh() {
    try {
      view = await api.external()
      error = null
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    }
  }
  onMount(() => {
    void refresh()
    const timer = setInterval(refresh, 5000)
    return () => clearInterval(timer)
  })

  const lanes = $derived(view ? visibleLanes(view.lanes, dismissed) : [])
  const hidden = $derived(view ? view.lanes.length - lanes.length : 0)
  const stopped = $derived(view?.stopped ?? [])

  function dismiss(lane: ExternalLane) {
    dismissed = prune({ ...dismissed, [laneKey(lane)]: lane.last_ms }, view?.lanes ?? [])
    saveDismissals(dismissed)
    if (open === laneKey(lane)) open = null
  }
  function showAll() {
    dismissed = {}
    saveDismissals(dismissed)
  }

  async function toggle(lane: ExternalLane) {
    const key = laneKey(lane)
    if (open === key) {
      open = null
      return
    }
    open = key
    log = []
    try {
      const { events } = await api.externalEvents(lane.channel)
      log = events.filter((e) => e.lane === lane.lane).slice(-20).reverse()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    }
  }

  async function start(channel: string) {
    starting = channel
    try {
      await api.externalStart(channel)
      await refresh()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      starting = ''
    }
  }

  // A call and its result read as the call; anything else (an approval asked
  // or decided, an item closed) says what it was.
  const describe = (e: ExternalEvent) => {
    const title = typeof e.payload.title === 'string' ? e.payload.title : ''
    const status = typeof e.payload.status === 'string' ? ` · ${e.payload.status}` : ''
    const state = typeof e.payload.state === 'string' ? ` · ${e.payload.state}` : ''
    if (e.kind === 'tool_call') return title
    if (e.kind === 'tool_result') return `↳ ${title}${status}`
    if (e.kind === 'pipeline_follow' && Array.isArray(e.payload.changes)) {
      return `pipeline ${e.payload.pipeline_id} · ${(e.payload.changes as string[]).join(' · ')}`
    }
    return `${e.kind.replaceAll('_', ' ')}${title ? ` · ${title}` : ''}${state}`
  }
</script>

{#if view?.enabled && (lanes.length || hidden || stopped.length)}
  <div class="h4">
    Your own agents <b>{lanes.length}</b>
    {#if hidden}<span class="r"><button class="lnk" onclick={showAll}>show {hidden} dismissed</button></span>{/if}
  </div>
  {#each stopped as channel (channel)}
    <div class="banner">
      broker access stopped on {channel} <b>· every external agent's calls on it are refused</b>
      <button class="lnk" onclick={() => start(channel)} disabled={starting !== ''}>Start broker access</button>
    </div>
  {/each}
  {#if error}<div class="banner err">{error}</div>{/if}
  <div class="rows">
    {#each lanes as lane (laneKey(lane))}
      {@const state = liveness(lane)}
      <Row gap={14} onclick={() => toggle(lane)} expanded={open === laneKey(lane)}>
        {#snippet lead()}<span class="dot {state}" title={state === 'running' ? `${lane.running} process${lane.running === 1 ? '' : 'es'} running` : state === 'gone' ? 'its process has exited' : 'it did not say which process it is'}></span>{/snippet}
        <span class="mono age" title="last call {new Date(lane.last_ms).toLocaleString()}">{formatAge(lane.last_ms, clock.now)}</span>
        <span class="t">
          {lane.lane ?? 'unlabelled'}
          <small>{lane.channel} · {lane.calls} call{lane.calls === 1 ? '' : 's'}{state === 'running' ? ' · running' : state === 'gone' ? ' · exited' : ''}{lane.pending ? ` · ${lane.pending} waiting on you` : ''}</small>
        </span>
        {#snippet actions()}<SideButton title="Hide until it calls again" onclick={() => dismiss(lane)}>dismiss</SideButton>{/snippet}
      </Row>
      {#if open === laneKey(lane)}
        <ol class="log">
          {#each log as e (e.seq)}
            <li><span class="mono">{formatAge(e.at_ms, clock.now)}</span> {@html codeSpans(describe(e))}</li>
          {:else}
            <li class="dim">nothing logged</li>
          {/each}
        </ol>
      {/if}
    {/each}
  </div>
{/if}

<style>
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  /* The lead's column is 10px, the dot 8px of it; the age's is 68px. */
  .dot {
    flex: none;
    margin-right: 2px;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--dim);
    opacity: 0.5;
  }
  .dot.running {
    background: var(--ok);
    opacity: 1;
  }
  .dot.gone {
    background: transparent;
    border: 1px solid var(--dim);
  }
  .age {
    flex: none;
    width: 68px;
  }
  .t {
    flex: 1;
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    margin-top: 2px;
  }
  .log {
    list-style: none;
    margin: 0 0 6px 24px;
    padding: 0;
    font: 12px var(--mono);
    color: var(--ink2);
  }
  .log li {
    padding: 2px 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .log .mono,
  .log .dim {
    color: var(--dim);
  }
  .h4 .r {
    margin-left: auto;
    font: 12.5px var(--sans);
    letter-spacing: 0;
    text-transform: none;
  }
  @media (max-width: 700px) {
    .age {
      display: none;
    }
  }
</style>
