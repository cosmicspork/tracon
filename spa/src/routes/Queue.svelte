<script lang="ts">
  import PermissionCard from '../components/PermissionCard.svelte'
  import PromotionCard from '../components/PromotionCard.svelte'
  import ReviewCard from '../components/ReviewCard.svelte'
  import SessionRow from '../components/SessionRow.svelte'
  import { store } from '../lib/store.svelte'

  let { only = null }: { only?: 'sessions' | null } = $props()

  const waiting = $derived(store.queue.waiting)
  const reviews = $derived(store.queue.reviews)
  const promotions = $derived(store.queue.promotions ?? [])
  const running = $derived(store.queue.running)
  const ended = $derived(store.queue.ended)
</script>

{#if !only}
  <div class="h4">
    Waiting on you <b>{waiting.length + reviews.length + promotions.length} · requests before reviews · oldest first</b>
  </div>
  {#if waiting.length === 0 && reviews.length === 0 && promotions.length === 0}
    <div class="empty">Nothing is waiting on you.</div>
  {:else}
    <div class="rows">
      <!-- Requests expire; reviews do not. Requests come first. -->
      {#each waiting as p (p.id)}
        <PermissionCard permission={p} />
      {/each}
      {#each reviews as r (r.id)}
        <ReviewCard review={r} />
      {/each}
      <!-- Batches last: they neither expire nor hold an agent. -->
      {#each promotions as p (p.id)}
        <PromotionCard promotion={p} />
      {/each}
    </div>
  {/if}
{/if}

<div class="h4">Running <b>{running.length}</b></div>
{#if running.length === 0}
  <div class="empty">No sessions running. <a href="/new">Start one.</a></div>
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
