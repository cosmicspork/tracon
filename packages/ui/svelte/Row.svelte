<script lang="ts">
  // A list line: the row (a link, a button, or plain when it does neither)
  // with an optional lead (a status dot, an icon), its content, and side
  // buttons after it. The row takes what the side buttons leave, all of the
  // line when there are none. State reads as the lead or a 3px bar plus a
  // wash, never an outline.
  import type { Snippet } from 'svelte'

  let {
    href,
    onclick,
    title,
    expanded,
    off = false,
    gap = 12,
    minHeight,
    lead,
    actions,
    children,
  }: {
    /** A link to open. */
    href?: string
    /** Or what a click does, making the row a button. */
    onclick?: (event: MouseEvent) => void
    title?: string
    /** For a row that opens something under it: aria-expanded. */
    expanded?: boolean
    /** Not available: dimmed, and neither link nor button. */
    off?: boolean
    /** Space between the lead and the content, in px. */
    gap?: number
    /** A floor for the row's height, in px. */
    minHeight?: number
    lead?: Snippet
    /** Side buttons (`SideButton`), after the row. */
    actions?: Snippet
    children?: Snippet
  } = $props()

  const style = $derived(`gap: 0 ${gap}px;${minHeight ? ` min-height: ${minHeight}px;` : ''}`)
</script>

<div class="tr-line">
  {#if href && !off}
    <a class="tr-row" {href} {title} {style}>{@render lead?.()}{@render children?.()}</a>
  {:else if onclick && !off}
    <button class="tr-row" type="button" {onclick} {title} aria-expanded={expanded} {style}
      >{@render lead?.()}{@render children?.()}</button
    >
  {:else}
    <div class="tr-row" class:off {title} aria-disabled={off || undefined} {style}>{@render lead?.()}{@render children?.()}</div>
  {/if}
  {@render actions?.()}
</div>

<style>
  .tr-line {
    display: flex;
    align-items: stretch;
    gap: 4px;
    min-width: 0;
  }
  .tr-row {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    padding: 10px 14px;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: inherit;
    font: inherit;
    text-align: left;
    text-decoration: none;
  }
  button.tr-row {
    cursor: pointer;
  }
  a.tr-row:hover,
  button.tr-row:hover {
    background: var(--s2);
  }
  .tr-row.off {
    color: var(--dim);
  }
</style>
