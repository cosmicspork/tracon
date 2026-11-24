<script lang="ts">
  // On a `[[repo]]` entry: the latest offer from another node accepted onto
  // it, with the sender's note, and the earlier ones behind "history". The
  // inbox above lists only offers still waiting (#1163).
  import { formatAge } from '../../lib/format'
  import { decidedLine, decidedOnto } from '../../lib/recipes'
  import type { RecipeOffer } from '../../lib/types'

  // `part` splits it where an entry's summary should carry only the latest
  // line and its open body the history.
  let {
    offers,
    path,
    part = 'all',
  }: { offers: RecipeOffer[]; path: string; part?: 'all' | 'latest' | 'history' } = $props()

  const decided = $derived(decidedOnto(offers, path))
  const when = (offer: RecipeOffer) => formatAge(offer.decided_ms ?? offer.received_ms)
</script>

{#if (part !== 'history' && decided.latest) || (part !== 'latest' && decided.earlier.length)}
  <div class="decided" class:history={part === 'history'}>
    {#if part !== 'history' && decided.latest}
      <small>{decidedLine(decided.latest)} {when(decided.latest)} ago</small>
      {#if decided.latest.note}<small class="said">“{decided.latest.note}”</small>{/if}
    {/if}
    {#if part !== 'latest' && decided.earlier.length}
      <details>
        <summary>history <b>{decided.earlier.length}</b></summary>
        {#each decided.earlier as offer (offer.id)}
          <small>{decidedLine(offer)} · received {formatAge(offer.received_ms)} ago</small>
          {#if offer.note}<small class="said">“{offer.note}”</small>{/if}
        {/each}
      </details>
    {/if}
  </div>
{/if}

<style>
  .decided { display: grid; gap: 2px; flex-basis: 100%; min-width: 0; }
  .decided.history { margin-bottom: 12px; }
  small { font-size: 12.5px; color: var(--dim); overflow-wrap: anywhere; }
  .said { color: var(--ink2); }
  details { display: grid; gap: 2px; }
  summary { cursor: pointer; font-size: 12.5px; color: var(--ink2); width: max-content; }
  details[open] { padding-top: 2px; }
  details small { display: block; }
</style>
