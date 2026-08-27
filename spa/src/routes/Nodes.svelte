<script lang="ts">
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'

  const nodes = $derived(store.nodes)
  const reachable = $derived(nodes.filter((n) => n.is_self || n.reachable).length)
  const meshed = $derived(store.mesh !== null && store.mesh.hub.state !== 'disabled')

  function running(id: string): number {
    return store.queue.running.filter((s) => s.node_id === id).length
  }
  function waiting(id: string): number {
    return store.queue.waiting.filter((p) => p.node_id === id).length + store.queue.reviews.filter((r) => r.node_id === id).length
  }
</script>

<div class="h4">
  Nodes
  <b
    >{#if meshed}{nodes.length} enrolled · {reachable} reachable{:else}this machine · no hub configured{/if}</b
  >
  {#if meshed && !surface.phone}
    <a class="lnk r" href="/nodes/enroll">Enroll a new node</a>
  {/if}
</div>

{#if nodes.length === 0}
  <div class="empty">Waiting for the node…</div>
{:else}
  <div class="rows">
    {#each nodes as node (node.id)}
      {@const off = !node.is_self && !node.reachable}
      <div class="node" class:bad={node.state === 'refused'} class:warn={node.harness.mismatch} class:off>
        <span class="bar"></span>
        <span class="nm">
          {node.name || node.id.slice(0, 8)}
          <small>{node.is_self ? 'this node · serving you' : 'peer'} · {node.id.slice(0, 4)}…{node.id.slice(-4)}</small>
        </span>
        <span class="st">
          {#if off}
            <span class="l off">
              <span class="chip off">unreachable</span> · {node.last_seen_ms
                ? `last seen ${formatAge(node.last_seen_ms, clock.now)}`
                : 'never heard from'}
            </span>
            <span
              >{running(node.id)} session{running(node.id) === 1 ? '' : 's'} show last known state{waiting(node.id)
                ? ` · ${waiting(node.id)} approval${waiting(node.id) === 1 ? '' : 's'} cannot be decided until it returns`
                : ''}</span
            >
          {:else if node.state === 'refused'}
            <span class="l bad">
              <span class="chip bad">refused</span> · {node.failed_check}: {node.failed_detail}
            </span>
            <span>Still serves its interface and relays. No sessions until the check passes.</span>
          {:else if node.harness.mismatch}
            <span class="l warn">
              <span class="chip warn">version mismatch</span> · node expects {node.harness.id}
              {node.harness.pinned}, host has {node.harness.found} · new sessions blocked
            </span>
          {:else if node.state === 'unknown'}
            <span class="l off"><span class="chip off">not yet heard from</span> · admitted, no hello yet</span>
          {:else}
            <span class="l">
              <span class="chip">ready</span> · boundary check passed · {node.harness.id}
              {node.harness.found ?? node.harness.pinned}
            </span>
            <span
              >{node.models.length} models offered · {running(node.id)} session{running(node.id) === 1 ? '' : 's'} running{store
                .channels.filter((c) => c.nodes.includes(node.id)).length
                ? ` · ${store.channels
                    .filter((c) => c.nodes.includes(node.id))
                    .map((c) => c.name)
                    .join(', ')}`
                : ''}</span
            >
          {/if}
        </span>
      </div>
    {/each}
  </div>
  {#if meshed && surface.phone}
    <div class="empty">Enrolling a node needs a desktop browser.</div>
  {/if}
{/if}

<style>
  .h4 .r {
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .node {
    display: grid;
    grid-template-columns: 3px 150px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    padding: 11px 14px 11px 0;
    overflow: hidden;
  }
  .bar {
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
  @media (max-width: 700px) {
    .node {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 4px 12px;
    }
    .st {
      grid-column: 2;
    }
  }
</style>
