<script lang="ts">
  let { diff }: { diff: string } = $props()

  // A unified diff is already structured text; the interface only needs to
  // colour it and keep it from scrolling the page sideways.
  const lines = $derived(diff.split('\n'))

  function kind(line: string): string {
    if (line.startsWith('+++') || line.startsWith('---')) return 'meta'
    if (line.startsWith('@@')) return 'hunk'
    if (line.startsWith('diff ') || line.startsWith('index ')) return 'meta'
    if (line.startsWith('+')) return 'add'
    if (line.startsWith('-')) return 'del'
    return 'ctx'
  }
</script>

<div class="diff">
  {#each lines as line, i (i)}
    <div class={kind(line)}>{line || ' '}</div>
  {/each}
</div>

<style>
  .diff {
    background: var(--s2);
    border-radius: 4px;
    font: 12px/1.55 var(--mono);
    overflow-x: auto;
    padding: 8px 0;
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
</style>
