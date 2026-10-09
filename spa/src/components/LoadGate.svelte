<script lang="ts">
  // The loading and failed states every list screen shares; `children` is the
  // answer, empty state included, and renders only once there is one. A
  // refresh that failed after an answer keeps it, under the error.
  import type { Snippet } from 'svelte'
  import { loadPhase, type LoadStatus } from '../lib/load'

  let {
    status,
    what,
    onretry,
    retrying = false,
    children,
  }: {
    status: LoadStatus
    /** What failed to load, in a phrase: "work", "documents". */
    what: string
    onretry?: () => void
    retrying?: boolean
    children?: Snippet
  } = $props()

  const phase = $derived(loadPhase(status))
</script>

{#if phase === 'failed' || phase === 'stale'}
  <div class="banner crit" role="alert">
    Could not {phase === 'stale' ? 'refresh' : 'load'} {what} <b>· {status.error}</b>
    {#if onretry}<button class="lnk" type="button" onclick={onretry} disabled={retrying}>{retrying ? 'Retrying…' : 'Retry'}</button>{/if}
  </div>
{/if}
{#if phase === 'loading'}
  <div class="empty" role="status">Loading {what}…</div>
{:else if phase === 'ready' || phase === 'stale'}
  {@render children?.()}
{/if}

<style>
  .banner b {
    overflow-wrap: anywhere;
  }
  .banner .lnk {
    margin-left: 8px;
    font-size: 12.5px;
  }
</style>
