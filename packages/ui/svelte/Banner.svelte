<script lang="ts">
  // `.banner`: a line across the page. `tone` crit is refusal and failure,
  // ok a confirmation, dim something quiet that persists; the default is
  // plain. `detail` is the bold part after the lead; `action` sits after it.
  import type { Snippet } from 'svelte'

  let {
    tone = '',
    detail,
    role,
    action,
    children,
  }: {
    tone?: '' | 'dim' | 'crit' | 'ok'
    detail?: string
    /** `alert` for a failure the reader should hear at once, `status` for news. */
    role?: 'alert' | 'status'
    action?: Snippet
    children?: Snippet
  } = $props()
</script>

<div class="banner {tone}" {role}>
  {@render children?.()}{#if detail}{' '}<b>· {detail}</b>{/if}{#if action}{' '}{@render action()}{/if}
</div>

<style>
  .banner b {
    overflow-wrap: anywhere;
  }
  .banner :global(.lnk) {
    margin-left: 8px;
    font-size: 12.5px;
  }
</style>
