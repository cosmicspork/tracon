<script lang="ts">
  // The quiet button beside a `Row`: dismiss, archive, open on another
  // channel. A link when it has `href`.
  import type { Snippet } from 'svelte'

  let {
    href,
    onclick,
    title,
    label,
    disabled = false,
    children,
  }: {
    href?: string
    onclick?: (event: MouseEvent) => void
    title?: string
    /** aria-label, when the text alone does not say what it acts on. */
    label?: string
    disabled?: boolean
    children?: Snippet
  } = $props()
</script>

{#if href}
  <a class="tr-side" {href} {title} aria-label={label}>{@render children?.()}</a>
{:else}
  <button class="tr-side" type="button" {onclick} {title} aria-label={label} {disabled}>{@render children?.()}</button>
{/if}

<style>
  .tr-side {
    flex: none;
    display: flex;
    align-items: center;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--dim);
    font: 12.5px var(--sans);
    padding: 0 12px;
    text-decoration: none;
    cursor: pointer;
  }
  .tr-side:hover:not(:disabled) {
    color: var(--ink);
    background: var(--s2);
  }
  .tr-side:disabled {
    cursor: default;
    opacity: 0.6;
  }
</style>
