<script lang="ts">
  import { attention } from '../lib/attention'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { nodeReadiness } from '../lib/nodes'
  import { store } from '../lib/store.svelte'

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

  function settingsLink(id: string): string {
    return `/settings?node=${encodeURIComponent(id)}#connections`
  }
</script>

<div class="h4">
  Nodes
  <b>{#if meshed}{nodes.length} enrolled · {reachable} reachable{:else}this machine · no hub configured{/if}</b>
  {#if meshed}<a class="lnk r" href="/nodes/enroll">Enroll a new node</a>{/if}
</div>

<p class="lede">Readiness and compatibility are reported here. Connections, credentials, notifications, and host configuration have one home in <a href="/settings#connections">Settings</a>.</p>

{#if nodes.length === 0}
  <div class="empty">Waiting for the node…</div>
{:else}
  <div class="rows">
    {#each nodes as node (node.id)}
      {@const readiness = nodeReadiness(node)}
      {@const off = !node.is_self && !node.reachable}
      <div class="node" class:bad={node.state === 'refused'} class:warn={node.harness.mismatch} class:off>
        <span class="bar"></span>
        <div class="head">
          <span class="nm">
            {node.name || node.id.slice(0, 8)}
            <small>{node.is_self ? 'serving node' : 'peer'} · {node.id.slice(0, 4)}…{node.id.slice(-4)}</small>
          </span>
          <span class="st">
            <span class="l" class:off={!readiness.canRun} class:bad={node.state === 'refused'} class:warn={node.harness.mismatch}>
              <span class="chip" class:off={!readiness.canRun} class:bad={node.state === 'refused'} class:warn={node.harness.mismatch}>{readiness.label}</span>
              {#if off && node.last_seen_ms}
                · last seen {formatAge(node.last_seen_ms, clock.now)}
              {:else}
                · {readiness.detail}
              {/if}
            </span>
            <span class="detail">
              {node.harness.id} {node.harness.found ?? node.harness.pinned} · {node.models.length} offered model{node.models.length === 1 ? '' : 's'} · {running(node.id)} running{waiting(node.id) ? ` · ${waiting(node.id)} awaiting an operator` : ''}
            </span>
          </span>
          <span class="actions">
            <a class="lnk" href={settingsLink(node.id)}>Manage connections</a>
            <a class="lnk" href="/metrics">Usage</a>
          </span>
        </div>
      </div>
    {/each}
  </div>
{/if}

<style>
  .h4 .r {
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
  }
  .lede {
    max-width: 62ch;
    margin: 0 0 10px;
    color: var(--ink2);
    font-size: 13.5px;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .node {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
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
    background: linear-gradient(90deg, var(--wash-crit), var(--s1) 42%);
  }
  .node.warn .bar {
    background: var(--wait);
  }
  .node.warn {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  /* Unreachable: dims, keeps its place, says when it was last seen. */
  .node.off .bar {
    background: var(--dim);
  }
  .node.off {
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 42%);
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
