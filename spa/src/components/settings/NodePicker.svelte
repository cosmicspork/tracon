<script lang="ts">
  // Which node a Settings card acts on: this one, or a peer administered
  // from here. Shown only when there is a peer to pick. A peer runs what the
  // card does only if its `[mesh] admin_peers` names this node, and each
  // change is confirmed here with a prompt naming that peer.
  import { nodeChoices } from '../../lib/peer-admin'
  import { store } from '../../lib/store.svelte'

  let {
    value = $bindable(''),
    onchange,
  }: { value?: string; onchange?: (node: string) => void } = $props()

  const choices = $derived(nodeChoices(store.nodes))
</script>

{#if choices.length > 1}
  <label class="node" title="A peer must list this node in [mesh] admin_peers">
    Node
    <select
      aria-label="Node to administer"
      {value}
      onchange={(event) => {
        value = event.currentTarget.value
        onchange?.(value)
      }}
    >
      {#each choices as choice (choice.id)}<option value={choice.id}>{choice.label}</option>{/each}
    </select>
  </label>
{/if}

<style>
  .node {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 12.5px;
    color: var(--dim);
  }
</style>
