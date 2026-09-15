<script lang="ts">
  import type { Snippet } from 'svelte'

  let {
    title,
    note,
    tone,
    actions,
    children,
  }: {
    title: string
    note?: string
    tone?: 'acc' | 'crit'
    actions?: Snippet
    children?: Snippet
  } = $props()
</script>

<section class="card" class:acc={tone === 'acc'} class:crit={tone === 'crit'}>
  <header>
    <h3>{title}</h3>
    {#if actions}<div class="actions">{@render actions()}</div>{/if}
    {#if note}<p>{note}</p>{/if}
  </header>
  {@render children?.()}
</section>

<style>
  .card {
    display: grid;
    gap: 12px;
    align-content: start;
    min-width: 0;
    background: var(--s1);
    border-radius: 6px;
    padding: 16px 18px;
  }
  .card.acc {
    box-shadow: inset 3px 0 0 var(--acc);
  }
  .card.crit {
    box-shadow: inset 3px 0 0 var(--crit);
  }
  header {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: baseline;
    gap: 4px 12px;
  }
  h3 {
    margin: 0;
    font: 600 14.5px/1.3 var(--sans);
    color: var(--ink);
  }
  .actions {
    justify-self: end;
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 6px 12px;
  }
  p {
    grid-column: 1 / -1;
    margin: 0;
    max-width: 90ch;
    color: var(--ink2);
    font-size: 13px;
  }
  .card > :global(:is(button, .btn, .lnk)) {
    justify-self: start;
  }
  .card :global(.empty) {
    background: var(--s2);
  }
  @media (max-width: 700px) {
    .card {
      padding: 14px;
    }
  }
</style>
