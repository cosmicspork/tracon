<script lang="ts">
  import type { Snippet } from 'svelte'
  import { tick } from 'svelte'

  // Explanation a screen does not need to show to be used: a circled "?"
  // that opens it beside the control. Click or tap, never hover, so a phone
  // reads it the same way.
  let { label, children }: { label: string; children: Snippet } = $props()

  const id = $props.id()
  let open = $state(false)
  let root = $state<HTMLElement>()
  let button = $state<HTMLButtonElement>()
  let panel = $state<HTMLElement>()
  let shift = $state(0)

  async function toggle() {
    open = !open
    shift = 0
    if (!open) return
    await tick()
    // Keep the panel on screen when the icon sits near the right edge.
    const r = panel?.getBoundingClientRect()
    if (r && r.right > window.innerWidth - 8) shift = window.innerWidth - 8 - r.right
  }

  function outside(e: PointerEvent) {
    if (open && root && !root.contains(e.target as Node)) open = false
  }

  function key(e: KeyboardEvent) {
    if (open && e.key === 'Escape') {
      open = false
      button?.focus()
    }
  }
</script>

<svelte:window onpointerdown={outside} onkeydown={key} />

<span class="help" bind:this={root}>
  <button
    bind:this={button}
    type="button"
    aria-label={label}
    aria-expanded={open}
    aria-controls={id}
    onclick={toggle}
  >
    <svg viewBox="0 0 16 16" aria-hidden="true"
      ><circle cx="8" cy="8" r="6.75" /><path d="M6.2 6.2a1.8 1.8 0 1 1 2.6 1.6c-.5.3-.8.6-.8 1.2v.3" /><circle
        class="dot"
        cx="8"
        cy="11.4"
        r=".55"
      /></svg
    >
  </button>
  {#if open}
    <span class="panel" {id} role="note" bind:this={panel} style:translate="{shift}px 0">
      {@render children()}
    </span>
  {/if}
</span>

<style>
  .help {
    position: relative;
    display: inline-flex;
    vertical-align: middle;
  }
  button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 2px;
    border: 0;
    background: none;
    color: var(--dim);
    cursor: pointer;
    border-radius: 50%;
  }
  button:hover,
  button[aria-expanded='true'] {
    color: var(--acc);
  }
  svg {
    width: 15px;
    height: 15px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.3;
    stroke-linecap: round;
  }
  .dot {
    fill: currentColor;
    stroke: none;
  }
  .panel {
    position: absolute;
    top: calc(100% + 6px);
    left: -8px;
    z-index: 20;
    width: max-content;
    max-width: min(320px, calc(100vw - 16px));
    padding: 10px 12px;
    background: var(--s3);
    border: 1px solid var(--rule);
    border-radius: 6px;
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.25);
    font: 400 12.5px/1.5 var(--sans);
    color: var(--ink2);
    text-transform: none;
    letter-spacing: normal;
    white-space: normal;
    text-align: left;
  }
  @media (max-width: 700px) {
    /* A touch target, without pushing the label it sits beside apart. */
    button {
      padding: 8px;
      margin: -6px;
    }
  }
</style>
