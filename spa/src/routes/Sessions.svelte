<script lang="ts">
  // Every session this node knows about. The home shows what landed lately;
  // this is where the rest of it lives.
  import SessionRow from '../components/SessionRow.svelte'
  import { store } from '../lib/store.svelte'

  const running = $derived(store.queue.running)
  const ended = $derived(store.queue.ended)
</script>

<div class="h4">Running <b>{running.length}</b></div>
{#if running.length === 0}
  <div class="empty">No sessions running. <a href="/">Start one.</a></div>
{:else}
  <div class="rows">
    {#each running as s (s.id)}
      <SessionRow session={s} />
    {/each}
  </div>
{/if}

<div class="h4">Ended <b>{ended.length}</b></div>
{#if ended.length === 0}
  <div class="empty">Nothing has ended yet.</div>
{:else}
  <div class="rows">
    {#each ended as s (s.id)}
      <SessionRow session={s} />
    {/each}
  </div>
{/if}

<style>
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
</style>
