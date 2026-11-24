<script lang="ts">
  // `.h4`: a section's heading, with a count or short detail in mono after
  // it and anything right-aligned (links, a filter) in `aside`.
  import type { Snippet } from 'svelte'

  let {
    detail,
    aside,
    children,
  }: { detail?: string | number; aside?: Snippet; children?: Snippet } = $props()
</script>

<div class="h4">
  {@render children?.()}{#if detail !== undefined && detail !== ''}{' '}<b>{detail}</b>{/if}
  {#if aside}<span class="tr-aside">{@render aside()}</span>{/if}
</div>

<style>
  .tr-aside {
    margin-left: auto;
    display: flex;
    gap: 14px;
    align-items: baseline;
    font: 12.5px var(--sans);
    letter-spacing: 0;
    text-transform: none;
  }
</style>
