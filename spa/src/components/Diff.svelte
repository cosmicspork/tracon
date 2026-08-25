<script lang="ts">
  import { classifyLines, splitDiff } from '../lib/diff'

  let { diff, perFile = false }: { diff: string; perFile?: boolean } = $props()

  const files = $derived(splitDiff(diff))
  const whole = $derived(classifyLines(diff.split('\n')))
</script>

{#if perFile}
  <!-- The phone reads a diff one file at a time: the list decides most reviews,
       and the hunks are there when they do not. -->
  <div class="files">
    {#each files as f (f.path)}
      <details>
        <summary>
          <span class="path">{f.path}</span>
          <span class="stat">+{f.added} −{f.removed}</span>
        </summary>
        <div class="diff">
          {#each classifyLines(f.lines) as line, i (i)}
            <div class={line.kind}>{line.text || ' '}</div>
          {/each}
        </div>
      </details>
    {/each}
  </div>
{:else}
  <div class="diff whole">
    {#each whole as line, i (i)}
      <div class={line.kind}>{line.text || ' '}</div>
    {/each}
  </div>
{/if}

<style>
  .diff {
    background: var(--s2);
    border-radius: 4px;
    font: 12px/1.55 var(--mono);
    overflow-x: auto;
    padding: 8px 0;
  }
  .diff.whole {
    max-height: 480px;
    overflow-y: auto;
  }
  .diff div {
    padding: 0 12px;
    white-space: pre;
  }
  .meta,
  .hunk {
    color: var(--dim);
  }
  .add {
    background: color-mix(in srgb, var(--ok) 16%, transparent);
  }
  .del {
    background: color-mix(in srgb, var(--crit) 16%, transparent);
  }
  .files {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  summary {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    align-items: baseline;
    padding: 10px 12px;
    background: var(--s1);
    border-radius: 4px;
    cursor: pointer;
    list-style: none;
    font: 12.5px var(--mono);
  }
  summary::-webkit-details-marker {
    display: none;
  }
  summary::before {
    content: '▸';
    color: var(--dim);
  }
  details[open] summary::before {
    content: '▾';
  }
  .path {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--ink);
  }
  .stat {
    color: var(--dim);
    white-space: nowrap;
  }
  details[open] .diff {
    margin-top: 4px;
  }
</style>
