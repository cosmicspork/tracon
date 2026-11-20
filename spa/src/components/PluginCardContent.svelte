<script lang="ts">
  import Diff from './Diff.svelte'
  import { render } from '../lib/markdown'
  import type { PluginBlock } from '../lib/plugin-card'
  import { wordCounts, wordDiff } from '../lib/word-diff'

  let { blocks }: { blocks: PluginBlock[] } = $props()
</script>

{#each blocks as b, i (i)}
  <section class="block">
    {#if b.title}<div class="bt">{b.title}</div>{/if}
    {#if b.format === 'markdown'}
      <article class="md">{@html render(b.text)}</article>
    {:else if b.format === 'diff'}
      <Diff diff={b.text} />
    {:else if b.format === 'table'}
      <dl class="kv">
        {#each b.rows as row, j (j)}
          <dt>{row.key}</dt>
          <dd>{row.value || '—'}</dd>
        {/each}
      </dl>
    {:else if b.format === 'word_diff'}
      {@const parts = wordDiff(b.before, b.after)}
      {@const counts = wordCounts(parts)}
      <div class="wd" aria-label="Word diff: {counts.removed} words out, {counts.added} in">
        {#if counts.added === 0 && counts.removed === 0}<span class="none">no change · </span>{/if}{#each parts as p, j (j)}{#if p.kind === 'add'}<ins>{p.text}</ins>{:else if p.kind === 'del'}<del>{p.text}</del>{:else}{p.text}{/if}{/each}
      </div>
      <details class="sides">
        <summary>Before and after · −{counts.removed} +{counts.added} words</summary>
        <div class="pair">
          <div class="side"><span>before</span><pre class="src">{b.before}</pre></div>
          <div class="side"><span>after</span><pre class="src">{b.after}</pre></div>
        </div>
      </details>
    {:else}
      <pre class="src" data-language={b.language ?? null}>{b.text}</pre>
    {/if}
  </section>
{/each}

<style>
  .block {
    margin: 12px 0;
  }
  .bt {
    color: var(--t2);
    font-size: 12px;
    margin-bottom: 6px;
  }
  .src {
    background: var(--s2);
    border-radius: 4px;
    font: 12px/1.55 var(--mono);
    margin: 0;
    overflow-x: auto;
    padding: 8px 12px;
    white-space: pre-wrap;
  }
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
  .wd .none {
    color: var(--t2);
  }
  .sides {
    margin-top: 6px;
  }
  .sides summary {
    color: var(--t2);
    cursor: pointer;
    font-size: 12px;
  }
  .pair {
    display: grid;
    gap: 8px;
    grid-template-columns: 1fr 1fr;
  }
  @media (max-width: 640px) {
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
  .wd .none {
    color: var(--t2);
  }
  .sides {
    margin-top: 6px;
  }
  .sides summary {
    color: var(--t2);
    cursor: pointer;
    font-size: 12px;
  }
  .pair {
      grid-template-columns: 1fr;
    }
  }
  .side span {
    color: var(--t2);
    font-size: 11px;
  }
  .kv {
    display: grid;
    gap: 4px 12px;
    grid-template-columns: max-content 1fr;
    margin: 0;
  }
  .kv dt {
    color: var(--t2);
  }
  .kv dd {
    margin: 0;
    white-space: pre-wrap;
  }
</style>
