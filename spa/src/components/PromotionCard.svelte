<script lang="ts">
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { promotionItems, type Promotion } from '../lib/types'

  let { promotion }: { promotion: Promotion } = $props()

  const items = $derived(promotionItems(promotion))
  const decided = $derived.by(() => {
    try {
      return Object.keys(JSON.parse(promotion.verdicts_json ?? '{}')).length
    } catch {
      return 0
    }
  })
  const kinds = $derived.by(() => {
    const n = new Map<string, number>()
    for (const i of items) n.set(i.kind, (n.get(i.kind) ?? 0) + 1)
    return [...n].map(([k, c]) => `${c} ${k}${c === 1 ? '' : 's'}`).join(', ')
  })
</script>

<!-- After reviews: a batch neither expires nor blocks an agent. -->
<a class="row" href="/promotions/{promotion.id}">
  <span class="bar"></span>
  <span class="mono">{formatAge(promotion.created_ms, clock.now)}</span>
  <span class="t">
    <em>Memory batch</em>
    {items.length} proposed for {promotion.channel}
    <small>{kinds}{decided ? ` · ${decided} of ${items.length} decided` : ''}</small>
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
