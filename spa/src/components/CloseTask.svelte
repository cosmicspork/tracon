<script lang="ts">
  // Closing the task a review's change belongs to. The operator's call alone:
  // no session closes the item it holds, so the task stays open through CI
  // and review. Once the followed request is merged, closing is what is left
  // to do, and it is offered prominently.
  import { api } from '../lib/api'
  import { closeOffer } from '../lib/continuation'
  import type { Continuation } from '../lib/types'

  let { itemId }: { itemId: string } = $props()

  let view = $state<Continuation | null>(null)
  let busy = $state(false)
  let error = $state<string | null>(null)

  async function load() {
    try {
      view = await api.workContinuation(itemId)
    } catch {
      view = null
    }
  }
  $effect(() => {
    void itemId
    void load()
  })

  const offer = $derived(view ? closeOffer(view) : null)

  async function close() {
    if (busy) return
    busy = true
    error = null
    try {
      await api.closeTask(itemId)
      await load()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

{#if offer}
  <div class="close" class:prominent={offer.prominent}>
    {#if offer.text}<span class="t">{offer.text}</span><span class="sep">·</span>{/if}
    <button class={offer.prominent ? 'btn p' : 'lnk'} disabled={busy} onclick={close}>Close task</button>
    {#if error}<span class="err">refused · {error}</span>{/if}
  </div>
{/if}

<style>
  .close {
    display: flex;
    gap: 8px;
    align-items: baseline;
    flex-wrap: wrap;
    margin: 8px 0;
    min-width: 0;
  }
  .close.prominent {
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 12px;
  }
  .t {
    color: var(--ink);
  }
  .sep {
    color: var(--dim);
  }
  .err {
    color: var(--crit);
    font-size: 13px;
    overflow-wrap: anywhere;
  }
</style>
