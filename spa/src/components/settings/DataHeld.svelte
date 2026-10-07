<script lang="ts">
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import { countLabel, formatBytes } from '../../lib/data'
  import { store } from '../../lib/store.svelte'
  import type { DataInventory } from '../../lib/types'

  let data = $state<DataInventory | null>(null)
  let error = $state('')
  let busy = $state(false)

  async function load() {
    busy = true
    try {
      data = await api.data()
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = false
    }
  }

  onMount(() => void load())
</script>

<Card
  title="Data on this node"
  note={`What ${store.node?.name ?? 'the serving node'} holds. It keeps everything until you delete it; each kind says where it is deleted, if it can be, and what a delete does beyond this node.`}
>
  {#snippet actions()}
    <button class="btn" onclick={() => void load()} disabled={busy}>{busy ? 'Counting…' : 'Count again'}</button>
  {/snippet}
  {#if error}
    <p class="crit">{error}</p>
  {:else if !data}
    <p class="dim">Counting…</p>
  {:else}
    <p class="total">
      <b>{formatBytes(data.total_bytes)}</b> in all · the database is {formatBytes(data.database_bytes)}, free pages included
    </p>
    <ul>
      {#each data.holdings as h (h.kind)}
        <li>
          <div class="line">
            <span class="kind">{h.label}</span>
            <span class="weight">{countLabel(h.count, h.unit)} · {formatBytes(h.bytes)}</span>
          </div>
          <p class="how">
            {#if h.delete}
              Delete in <a href={h.delete.path}>{h.delete.label}</a>.
            {:else}
              No delete.
            {/if}
            {h.propagation}
          </p>
        </li>
      {/each}
    </ul>
    <p class="dim">Rows weigh what their values carry; directories weigh what their files take on disk.</p>
  {/if}
</Card>

<style>
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 10px;
  }
  .line {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    flex-wrap: wrap;
  }
  .kind {
    font-weight: 600;
  }
  .weight {
    color: var(--ink2);
    font-variant-numeric: tabular-nums;
  }
  .how {
    margin: 2px 0 0;
    color: var(--ink2);
    font-size: 0.9em;
  }
  .total {
    margin: 0;
  }
  .dim {
    color: var(--dim);
    margin: 0;
  }
  .crit {
    color: var(--crit);
    margin: 0;
  }
</style>
