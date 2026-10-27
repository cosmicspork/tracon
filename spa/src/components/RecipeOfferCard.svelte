<script lang="ts">
  import { clock } from '../lib/clock.svelte'
  import { formatAge, formatDuration } from '../lib/format'
  import { OFFERS_LINK } from '../lib/recipes'
  import type { RecipeOffer } from '../lib/types'

  let { offer }: { offer: RecipeOffer } = $props()
</script>

<!-- Accepting writes node.toml after a review of what changes, so it stays in
     Settings; this only says an offer is there and routes to it. -->
<a class="row" href={OFFERS_LINK}>
  <span class="bar"></span>
  <span class="mono">{formatAge(offer.received_ms, clock.now)}</span>
  <span class="t">
    <em>Recipe offer</em>
    {offer.sender_name} offers a recipe for <code>{offer.repo}</code>
    <small>on {offer.channel} · lapses in {formatDuration(Math.max(0, offer.expires_ms - clock.now))} </small>
  </span>
  <span class="act">Review</span>
</a>

<style>
  .row {
    display: grid;
    grid-template-columns: 3px 72px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
    border-radius: 4px;
    padding: 11px 14px 11px 0;
    color: inherit;
    text-decoration: none;
    overflow: hidden;
  }
  .bar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--wait);
  }
  .mono {
    font: 12.5px var(--mono);
    color: var(--dim);
    white-space: nowrap;
  }
  .t {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .t em {
    font-style: normal;
    color: var(--wait);
    margin-right: 6px;
  }
  .t code {
    font: 13px var(--mono);
  }
  .t small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
  }
  .act {
    font: 12.5px var(--mono);
    color: var(--acc);
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: 3px minmax(0, 1fr) auto;
    }
    .mono {
      display: none;
    }
  }
</style>
