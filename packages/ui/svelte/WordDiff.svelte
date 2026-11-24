<script lang="ts">
  // A text before and after, diffed by word: what was taken out struck
  // through on a red wash, what was put in on a green one. Drawn by the page
  // from the two texts, so what is shown is exactly the change.
  import { wordCounts, wordDiff } from '../word-diff'

  let { before, after }: { before: string; after: string } = $props()

  const parts = $derived(wordDiff(before, after))
  const counts = $derived(wordCounts(parts))
</script>

<div class="wd" aria-label="Word diff: {counts.removed} words out, {counts.added} in">
  {#if counts.added === 0 && counts.removed === 0}<span class="none">no change · </span>{/if}{#each parts as p, j (j)}{#if p.kind === 'add'}<ins>{p.text}</ins>{:else if p.kind === 'del'}<del>{p.text}</del>{:else}{p.text}{/if}{/each}
</div>

<style>
  .wd {
    background: var(--s2);
    border-radius: 4px;
    line-height: 1.6;
    padding: 8px 12px;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .wd ins {
    background: color-mix(in srgb, var(--ok) 25%, transparent);
    text-decoration: none;
  }
  .wd del {
    background: color-mix(in srgb, var(--crit) 20%, transparent);
    text-decoration: line-through;
  }
</style>
