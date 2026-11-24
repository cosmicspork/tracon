<script lang="ts">
  // `.empty`: nothing to show yet, or nothing at all, said in words, with an
  // optional way forward. `loading` marks it a live status.
  import type { Snippet } from 'svelte'

  let { loading = false, action, children }: { loading?: boolean; action?: Snippet; children?: Snippet } = $props()
</script>

<div class="empty" role={loading ? 'status' : undefined}>
  {@render children?.()}{#if action}{' '}{@render action()}{/if}
</div>
