<script lang="ts">
  // What a review publishes after, each linked with where it stands, and what
  // publishes after it (`submit_review`'s `after`). Nothing when it is in no
  // such order.
  import Name from './Name.svelte'
  import type { ReviewDependencies } from '../lib/types'

  let { dependencies }: { dependencies: ReviewDependencies | null | undefined } = $props()

  const after = $derived(dependencies?.after ?? [])
  const before = $derived(dependencies?.before ?? [])
</script>

{#if after.length > 0 || before.length > 0}
  <div class="deps m">
    {#each after as d (d.review_id)}
      <span class:ok={d.satisfied} class:wait={!d.satisfied}
        >after <Name kind="review" id={d.review_id} href="/reviews/{d.review_id}" hint={d.title ?? undefined} /> · {d.says}</span
      >
    {/each}
    {#each before as d (d.review_id)}
      <span>before <Name kind="review" id={d.review_id} href="/reviews/{d.review_id}" hint={d.title ?? undefined} /></span>
    {/each}
  </div>
{/if}

<style>
  .deps {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 14px;
    font: 12px var(--mono);
    color: var(--dim);
    margin: 6px 0 0;
  }
  .ok {
    color: var(--ok);
  }
  .wait {
    color: var(--wait);
  }
</style>
