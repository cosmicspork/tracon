<script lang="ts">
  import { store } from '../lib/store.svelte'

  const node = $derived(store.node)
</script>

<div class="h4">Nodes <b>this machine; the mesh arrives in Phase 2</b></div>

{#if !node}
  <div class="empty">Waiting for the node…</div>
{:else}
  <div class="node" class:bad={node.state === 'refused'} class:warn={node.harness.mismatch}>
    <span class="bar"></span>
    <span class="nm">
      {node.name}
      <small>this node · serving you</small>
    </span>
    <span class="st">
      {#if node.state === 'refused'}
        <span class="l bad">
          <span class="chip bad">refused</span> · {node.failed_check}: {node.failed_detail}
        </span>
        <span>Still serves this interface. No sessions until the check passes.</span>
      {:else if node.harness.mismatch}
        <span class="l warn">
          <span class="chip warn">version mismatch</span> · node expects {node.harness.id}
          {node.harness.pinned}, host has {node.harness.found} · new sessions blocked
        </span>
      {:else}
        <span class="l">
          <span class="chip">ready</span> · boundary check passed · {node.harness.id}
          {node.harness.found ?? node.harness.pinned}
        </span>
        <span
          >{node.models.length} models offered · {store.queue.running.length} session{store.queue
            .running.length === 1
            ? ''
            : 's'} running</span
        >
      {/if}
    </span>
  </div>
{/if}

<style>
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
  .nm {
    font-weight: 600;
  }
  .nm small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    font-weight: 400;
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
  .chip.warn::before {
    background: var(--wait);
  }
</style>
