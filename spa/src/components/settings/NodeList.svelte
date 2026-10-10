<script lang="ts">
  // Every node this one knows, with what decides whether it can take work:
  // readiness, harness versions, what runs there and what waits on an
  // operator. It was a screen of its own; on one node it was a single row.
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import { attention } from '../../lib/attention'
  import { formatBytes } from '../../lib/data'
  import { clock } from '../../lib/clock.svelte'
  import { formatAge } from '../../lib/format'
  import { nodeHarnesses, nodeReadiness } from '../../lib/nodes'
  import { store } from '../../lib/store.svelte'

  const nodes = $derived(store.nodes)
  const reachable = $derived(nodes.filter((node) => node.is_self || node.reachable).length)
  const meshed = $derived(store.mesh !== null && store.mesh.hub.state !== 'disabled')

  function running(id: string): number {
    return store.queue.running.filter((session) => session.node_id === id).length
  }

  // Decisions only, for the same reason the rail counts decisions: a review
  // the agent is revising on that node is not an operator waiting.
  function waiting(id: string): number {
    return attention({
      permissions: store.queue.waiting.filter((permission) => permission.node_id === id),
      reviews: store.queue.reviews.filter((review) => review.node_id === id),
      nodes: store.nodes,
      mesh: store.mesh,
      now: clock.now,
    }).count
  }

  // What the serving node holds; a peer's storage is its own to report.
  let held = $state<number | null>(null)
  onMount(() => {
    api.data().then((data) => (held = data.total_bytes), () => (held = null))
  })

  function settingsLink(id: string): string {
    return `/settings?node=${encodeURIComponent(id)}#connections`
  }
</script>

<Card
  title="Nodes"
  note={meshed
    ? `${nodes.length} enrolled · ${reachable} reachable. Readiness and harness versions as each node reports them.`
    : 'This machine only; pair a hub above to add others. Readiness and harness versions as this node reports them.'}
>
  {#snippet actions()}
    {#if meshed}<a class="lnk" href="/nodes/enroll">Enroll a new node</a>{/if}
  {/snippet}
  {#if nodes.length === 0}
    <div class="empty">Waiting for the node…</div>
  {:else}
    <div class="rows">
      {#each nodes as node (node.id)}
        {@const readiness = nodeReadiness(node)}
        {@const off = !node.is_self && !node.reachable}
        {@const mismatch = nodeHarnesses(node).some((h) => h.mismatch)}
        <div class="node" class:bad={node.state === 'refused'} class:warn={mismatch} class:off>
          <span class="bar"></span>
          <div class="head">
            <span class="nm">
              {node.name || node.id.slice(0, 8)}
              <small>{node.is_self ? 'serving node' : 'peer'} · {node.id.slice(0, 4)}…{node.id.slice(-4)}</small>
            </span>
            <span class="st">
              <span class="l" class:off={!readiness.canRun} class:bad={node.state === 'refused'} class:warn={mismatch}>
                <span class="chip" class:off={!readiness.canRun} class:bad={node.state === 'refused'} class:warn={mismatch}>{readiness.label}</span>
                {#if off && node.last_seen_ms}
                  · last seen {formatAge(node.last_seen_ms, clock.now)}
                {:else}
                  · {readiness.detail}
                {/if}
              </span>
              <span class="detail">
                {nodeHarnesses(node).map((h) => `${h.id} ${h.found ?? h.pinned}${h.mismatch ? ' (mismatch)' : ''}`).join(' · ')} · {node.models.length} offered model{node.models.length === 1 ? '' : 's'} · {running(node.id)} running{waiting(node.id) ? ` · ${waiting(node.id)} awaiting an operator` : ''}{#if node.is_self && held !== null} · holds <a class="lnk" href="/settings#data">{formatBytes(held)}</a>{/if}
              </span>
            </span>
            <span class="actions">
              <a class="lnk" href={settingsLink(node.id)}>Manage connections</a>
              <a class="lnk" href="/usage">Usage</a>
            </span>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</Card>

<style>
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .node {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s2);
    border-radius: 4px;
    overflow: hidden;
  }
  .bar {
    grid-row: 1 / span 2;
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--ok);
  }
  .node.bad .bar {
    background: var(--crit);
  }
  .node.bad {
    background: linear-gradient(90deg, var(--wash-crit), var(--s2) 42%);
  }
  .node.warn .bar {
    background: var(--wait);
  }
  .node.warn {
    background: linear-gradient(90deg, var(--wash-wait), var(--s2) 42%);
  }
  /* Unreachable: dims, keeps its place, says when it was last seen. */
  .node.off .bar {
    background: var(--dim);
  }
  .node.off {
    background: linear-gradient(90deg, var(--wash-dim), var(--s2) 42%);
  }
  .node.off .nm,
  .node.off .st {
    color: var(--dim);
  }
  .head {
    display: grid;
    grid-template-columns: 150px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    padding: 11px 14px 11px 0;
    color: var(--ink);
    min-width: 0;
  }
  .nm {
    font-weight: 600;
    min-width: 0;
  }
  .nm small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    font-weight: 400;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .st {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    min-width: 0;
  }
  .st span {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .st .l.bad {
    color: var(--crit);
  }
  .st .l.warn {
    color: var(--wait);
  }
  .st .l.off {
    color: var(--dim);
  }
  .detail {
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .actions {
    display: flex;
    gap: 12px;
    align-items: center;
    white-space: nowrap;
  }
  .node.bad .detail {
    color: var(--crit);
  }
  .node.warn .detail {
    color: var(--wait);
  }
  @media (max-width: 700px) {
    .head {
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 4px 12px;
    }
    .st {
      grid-column: 1;
    }
    .actions {
      grid-column: 1;
      flex-wrap: wrap;
    }
  }
</style>
