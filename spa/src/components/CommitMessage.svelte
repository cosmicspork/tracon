<script lang="ts">
  // A commit message box: soft-wrapped to its width, with a gutter that
  // numbers each real line once, at its first visual row, so a wrapped
  // continuation reads apart from a line the message actually breaks. A
  // textarea cannot number its own visual lines, so a hidden mirror lays the
  // same text out at the same width and the numbers sit at its lines'
  // offsets. Git's conventions are shown, never enforced: a faint guide at
  // column 72 when the box is that wide, and a note when the subject runs
  // past it. The value is the textarea's own; nothing here writes to it.
  import { onMount } from 'svelte'

  import { autogrow } from '../lib/autogrow'
  import { COMMIT_COLUMN, longSubject, realLines } from '../lib/linegutter'

  let { value = $bindable(''), label, disabled = false }: { value?: string; label: string; disabled?: boolean } = $props()
  const uid = $props.id()

  let box = $state<HTMLTextAreaElement>()
  let mirror = $state<HTMLDivElement>()
  let ch = $state<HTMLSpanElement>()
  /** Each real line's first visual row, from the top of the text. */
  let tops = $state<number[]>([])
  let scroll = $state(0)
  /** Whether the box holds the convention's column, so its guide means something. */
  let wide = $state(false)
  let guide = $state(0)

  const lines = $derived(realLines(value))
  const subject = $derived(longSubject(value))
  const digits = $derived(Math.max(2, String(lines.length).length))

  function measure() {
    if (!box || !mirror) return
    const style = getComputedStyle(box)
    const pad = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight)
    mirror.style.width = `${box.clientWidth}px`
    const col = ch?.getBoundingClientRect().width ?? 0
    wide = col > 0 && box.clientWidth - pad >= COMMIT_COLUMN * col
    guide = parseFloat(style.paddingLeft) + COMMIT_COLUMN * col
    tops = [...mirror.querySelectorAll<HTMLElement>('.ln')].map((l) => l.offsetTop)
    scroll = box.scrollTop
  }

  $effect(() => {
    void lines
    measure()
  })

  onMount(() => {
    if (!box) return
    const seen = new ResizeObserver(measure)
    seen.observe(box)
    // The mono face can arrive after the first layout and change every width.
    document.fonts?.ready.then(measure)
    return () => seen.disconnect()
  })
</script>

<div class="cm" class:wide style:--guide="{guide}px" style:--digits={digits}>
  <div class="gutter" aria-hidden="true">
    {#each tops as top, i (i)}
      <span class:long={i === 0 && subject !== null} style:top="{top - scroll}px">{i + 1}</span>
    {/each}
  </div>
  <div class="field">
    <span class="ch" aria-hidden="true" bind:this={ch}>0</span>
    <!-- On one line: the mirror keeps white space, so any between its lines would be a line too. -->
    <div class="mirror" aria-hidden="true" bind:this={mirror}>{#each lines as line, i (i)}<div class="ln">{line || ' '}</div>{/each}</div>
    <textarea
      class="grow"
      bind:this={box}
      aria-label={label}
      aria-describedby={subject !== null ? `${uid}-hint` : undefined}
      bind:value
      use:autogrow={value}
      onscroll={() => (scroll = box?.scrollTop ?? 0)}
      {disabled}
    ></textarea>
  </div>
</div>
{#if subject !== null}
  <div class="hint" id="{uid}-hint" role="status">Subject is {subject} characters; git convention keeps it to {COMMIT_COLUMN}.</div>
{/if}

<style>
  .cm {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    background: var(--s1);
    border-radius: 4px;
    margin: 6px 0 4px;
    font: 12.5px/1.5 var(--mono);
  }
  .gutter {
    position: relative;
    overflow: hidden;
    width: calc(var(--digits) * 1ch + 14px);
    color: var(--dim);
    opacity: 0.7;
    text-align: right;
    user-select: none;
  }
  .gutter span {
    position: absolute;
    right: 6px;
  }
  .gutter span.long {
    color: var(--wait);
    opacity: 1;
  }
  .field {
    position: relative;
    min-width: 0;
  }
  /* The textarea's twin: same face, padding, width and wrapping, never seen.
     Clipped to the field, or a long message's hidden rows would still stretch
     the page's scroll height; clipping leaves its lines' offsets alone. */
  .mirror,
  textarea {
    box-sizing: border-box;
    padding: 9px 11px 9px 6px;
    font: inherit;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    word-break: normal;
    tab-size: 8;
  }
  .mirror {
    position: absolute;
    top: 0;
    bottom: 0;
    left: 0;
    overflow: hidden;
    visibility: hidden;
    pointer-events: none;
  }
  .ch {
    position: absolute;
    visibility: hidden;
  }
  textarea {
    display: block;
    width: 100%;
    margin: 0;
    border: 0;
    border-radius: 0 4px 4px 0;
    background: transparent;
    color: var(--ink);
    overflow-x: hidden;
  }
  .wide textarea {
    background: linear-gradient(to right, transparent var(--guide), var(--rule) var(--guide), var(--rule) calc(var(--guide) + 1px), transparent calc(var(--guide) + 1px)) no-repeat;
  }
  textarea:disabled {
    color: var(--ink2);
  }
  .hint {
    font: 12px var(--sans);
    color: var(--wait);
    margin: 0 0 4px;
  }
</style>
