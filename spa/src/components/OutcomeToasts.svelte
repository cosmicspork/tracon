<script lang="ts">
  // A call the operator allowed and then left, or that ran past the page's
  // wait, and did not run: said in the shell, wherever they are now, with
  // the way back to its approval. Its own page, if open, says it there.
  import { untrack } from 'svelte'
  import { ToastGate, TOAST_MS, type OutcomeToast } from '../lib/allowed-outcome'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'

  const gate = new ToastGate()
  let toasts = $state<OutcomeToast[]>([])

  $effect(() => {
    const path = router.path
    const seen = store.seenOutcomes
    for (const o of store.settled) {
      const toast = gate.notice(o, path, seen, Date.now())
      if (!toast) continue
      untrack(() => (toasts = [...toasts, toast]))
      setTimeout(() => dismiss(toast.id), TOAST_MS)
    }
  })

  // Opened another way (the feed, a link): the toast has said its piece.
  $effect(() => {
    const seen = store.seenOutcomes
    if (toasts.some((t) => seen.has(t.id))) toasts = toasts.filter((t) => !seen.has(t.id))
  })

  function dismiss(id: string) {
    toasts = toasts.filter((t) => t.id !== id)
  }
</script>

{#if toasts.length}
  <div class="toasts" role="status" aria-live="polite">
    {#each toasts as t (t.id)}
      <div class="toast">
        <a href={t.href} onclick={() => dismiss(t.id)}>{t.text}</a>
        <button type="button" class="x" aria-label="Dismiss" onclick={() => dismiss(t.id)}>×</button>
      </div>
    {/each}
  </div>
{/if}

<style>
  .toasts {
    position: fixed;
    right: 16px;
    bottom: 16px;
    z-index: 20;
    display: flex;
    flex-direction: column;
    gap: 6px;
    max-width: min(420px, calc(100vw - 32px));
  }
  .toast {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 10px 12px;
    background: var(--s1);
    border: 1px solid var(--rule);
    box-shadow: inset 3px 0 0 var(--crit);
    font: 13px var(--sans);
  }
  .toast a {
    flex: 1;
    color: var(--ink);
    text-decoration: none;
  }
  .toast a:hover {
    text-decoration: underline;
  }
  .x {
    background: none;
    border: 0;
    color: var(--dim);
    cursor: pointer;
    font: 15px/1 var(--sans);
    padding: 0;
  }
  @media (max-width: 700px) {
    .toasts {
      left: 12px;
      right: 12px;
      bottom: calc(68px + env(safe-area-inset-bottom));
      max-width: none;
    }
  }
</style>
