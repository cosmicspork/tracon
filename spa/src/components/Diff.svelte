<script lang="ts">
  import { classifyLines, splitDiff } from '../lib/diff'
  import { numberLines, type Numbered } from '../lib/reviewThreads'

  let {
    diff,
    perFile = false,
    onpick,
    marks,
  }: {
    diff: string
    perFile?: boolean
    /**
     * Given, each numbered line gets a gutter button that calls it: how a
     * review thread is started on a line. A diff component that replaces
     * this one calls it with the same `Numbered` line.
     */
    onpick?: (line: Numbered) => void
    /** How many threads a line carries, to mark it. */
    marks?: (line: Numbered) => number
  } = $props()

  const files = $derived(splitDiff(diff))
  const whole = $derived(numberLines(classifyLines(diff.split('\n'))))
  /** The line each file starts on, by index, so a file list can scroll to it. */
  const starts = $derived.by(() => {
    const at = new Map<number, string>()
    let n = 0
    whole.forEach((line, i) => {
      if (line.text.startsWith('diff --git ') && files[n]) at.set(i, files[n++].path)
    })
    return at
  })
</script>

{#if perFile}
  <!-- The phone reads a diff one file at a time: the list decides most reviews,
       and the hunks are there when they do not. -->
  <div class="files">
    {#each files as f (f.path)}
      <details data-path={f.path}>
        <summary>
          <span class="path">{f.path}</span>
          <span class="stat">+{f.added} −{f.removed}</span>
        </summary>
        <div class="diff">
          {#each numberLines(classifyLines(f.lines)) as line, i (i)}
            <div class={line.kind}>{@render gutter(line)}{line.text || ' '}</div>
          {/each}
        </div>
      </details>
    {/each}
  </div>
{:else}
  <div class="diff whole">
    {#each whole as line, i (i)}
      <div class={line.kind} data-path={starts.get(i)}>{@render gutter(line)}{line.text || ' '}</div>
    {/each}
  </div>
{/if}

{#snippet gutter(line: Numbered)}
  {#if onpick}
    {@const n = line.new ?? line.old}
    {@const marked = marks?.(line) ?? 0}
    {#if n !== null}
      <button
        type="button"
        class="gutter"
        class:marked={marked > 0}
        title={marked > 0 ? `${marked} thread${marked === 1 ? '' : 's'} here · add another` : 'Comment on this line'}
        onclick={() => onpick(line)}>{n}</button
      >
    {:else}
      <span class="gutter"></span>
    {/if}
  {/if}
{/snippet}

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
  .gutter {
    display: inline-block;
    width: 4ch;
    margin-right: 8px;
    padding: 0;
    border: 0;
    background: none;
    color: var(--dim);
    font: inherit;
    text-align: right;
    cursor: pointer;
  }
  span.gutter {
    cursor: default;
  }
  .gutter.marked {
    color: var(--ink);
    font-weight: 600;
    text-decoration: underline;
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
