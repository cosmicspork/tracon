<script lang="ts">
  // A publication attempt, one step per line, as it runs: what the node is
  // doing after Approve, ticking from pending to running to done, or stopping
  // with its reason, until it lands on the published change's link.
  import { clock } from '../lib/clock.svelte'
  import { fromView, latestProgress, stepRows } from '../lib/publicationSteps'
  import type { PublicationProgress, PublicationView } from '../lib/types'

  interface Props {
    reviewId: string
    /** The latest `publication` frame this tab heard for the review. */
    live: PublicationProgress | null
    /** The publication record the review was loaded with. */
    loaded: PublicationView | null
  }

  let { reviewId, live, loaded }: Props = $props()

  const progress = $derived(latestProgress(live, loaded ? fromView(loaded, reviewId) : null))
  const rows = $derived(progress ? stepRows(progress, clock.now) : [])
  const MARK = { done: '✓', running: '', failed: '✗', unknown: '?', pending: '' } as const
</script>

{#if rows.length}
  <ol class="pub-steps" aria-label="Publication" aria-live="polite">
    {#each rows as row (row.step)}
      <li class={row.state} data-step={row.step}>
        <span class="mark" aria-hidden="true">{#if row.state === 'running'}<span class="spin"></span>{:else}{MARK[row.state]}{/if}</span>
        <span class="label">
          {#if row.url}<a href={row.url} target="_blank" rel="noopener">{row.label}</a>{:else}{row.label}{/if}
          {#if row.elapsed}<span class="elapsed">{row.elapsed}</span>{/if}
        </span>
        {#if row.reason}<span class="reason">{row.reason}</span>{/if}
      </li>
    {/each}
  </ol>
{/if}

<style>
  .pub-steps {
    list-style: none;
    margin: 6px 0 12px;
    padding: 0;
    display: grid;
    gap: 4px;
    font-size: 13px;
  }
  li {
    display: grid;
    grid-template-columns: 18px minmax(0, 1fr);
    gap: 0 8px;
    align-items: baseline;
  }
  .mark {
    text-align: center;
    font-weight: 600;
  }
  .label {
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .done .mark {
    color: var(--ok);
  }
  .pending {
    color: var(--dim);
  }
  .failed .mark,
  .unknown .mark,
  .failed .label,
  .unknown .label {
    color: var(--crit);
  }
  .reason {
    grid-column: 2;
    color: var(--crit);
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }
  .elapsed {
    margin-left: 6px;
    color: var(--dim);
    font-variant-numeric: tabular-nums;
  }
  .spin {
    display: inline-block;
    width: 9px;
    height: 9px;
    border: 2px solid var(--dim);
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spin {
      animation: none;
      border-top-color: var(--dim);
    }
  }
</style>
