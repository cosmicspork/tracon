<script lang="ts">
  import { formatAge } from '../lib/format'
  import type { Review } from '../lib/types'

  let { review }: { review: Review } = $props()

  const noun = $derived(review.provider === 'gitlab' ? 'MR' : 'PR')
  const files = $derived.by(() => {
    try {
      return (JSON.parse(review.files) as unknown[]).length
    } catch {
      return 0
    }
  })
</script>

<a class="row" class:revising={review.state === 'revising'} href="/reviews/{review.id}">
  <span class="bar"></span>
  <span class="mono">{formatAge(review.created_ms)}</span>
  <span class="t">
    <em>{review.state === 'revising' ? 'Changes requested' : 'Review'}</em>
    {review.title}
    <small
      >{noun} · {files} files · +{review.added} −{review.removed} · {review.channel}{review.claimed_ms
        ? ' · claimed'
        : ''}</small
    >
  </span>
  <span class="act">Open</span>
</a>

<style>
  .row {
    display: grid;
    grid-template-columns: 3px 72px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
    text-decoration: none;
    color: inherit;
  }
  .bar {
    align-self: stretch;
    background: var(--wait);
    border-radius: 2px 0 0 2px;
  }
  /* Waiting on the agent, not on you: the accent, not amber. */
  .row.revising {
    background: linear-gradient(90deg, var(--wash-run), var(--s1) 42%);
  }
  .row.revising .bar {
    background: var(--acc);
  }
  .row.revising .t em {
    color: var(--acc);
  }
  .t {
    font-weight: 500;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--wait);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    margin-top: 2px;
  }
  .act {
    color: var(--acc);
    font-weight: 500;
    white-space: nowrap;
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: 3px minmax(0, 1fr);
    }
    .row > .mono,
    .act {
      display: none;
    }
  }
</style>
