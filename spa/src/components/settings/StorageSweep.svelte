<script lang="ts">
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import { store } from '../../lib/store.svelte'
  import type { StorageItem } from '../../lib/types'

  let items = $state<StorageItem[] | null>(null)
  let caches = $state(false)
  let confirming = $state(false)
  let busy = $state('')
  let error = $state('')
  let note = $state('')

  const local = $derived(store.node?.loopback ?? false)
  const removable = $derived(items?.filter((item) => item.remove) ?? [])
  const kept = $derived(items?.filter((item) => !item.remove) ?? [])

  async function sweep(apply: boolean) {
    if (busy) return
    busy = apply ? 'apply' : 'preview'
    error = ''
    note = ''
    try {
      const result = await api.storageSweep(apply, caches)
      if (apply) {
        const failed = result.items.filter((item) => item.remove && item.error)
        const removed = result.items.filter((item) => item.removed).length
        note = `Removed ${removed}.${failed.length ? ` ${failed.length} could not be removed; see below.` : ''}`
        items = failed.length ? result.items : (await api.storageSweep(false, caches)).items
      } else {
        items = result.items
      }
      confirming = false
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }
</script>

<Card
  title="Runtime storage"
  note="Volumes and state directories whose session, workspace or check is over. A session's workspace and harness state outlive it until it is archived; archiving and removing here gives up resuming, exporting and restoring it."
>
  {#if !local}
    <p class="blocked">Storage is reclaimed on the node itself.</p>
  {/if}
  <div class="controls">
    <button class="btn" onclick={() => void sweep(false)} disabled={busy !== '' || !local}>
      {busy === 'preview' ? 'Looking…' : items ? 'Look again' : 'Find reclaimable storage'}
    </button>
    <label><input type="checkbox" bind:checked={caches} disabled={busy !== ''} /> Include dependency caches (rebuilt when next needed)</label>
  </div>

  {#if items}
    {#if removable.length}
      <ul>
        {#each removable as item (item.kind + item.name)}
          <li class:failed={item.error}>
            <code>{item.name}</code>
            <small>{item.kind} · {item.error ? `could not remove: ${item.error}` : item.reason}</small>
          </li>
        {/each}
      </ul>
      {#if confirming}
        <div class="confirm">
          <p>Remove {removable.length} item{removable.length === 1 ? '' : 's'}? This cannot be undone.</p>
          <button class="btn d" onclick={() => void sweep(true)} disabled={busy !== '' || !local}>{busy === 'apply' ? 'Removing…' : `Remove ${removable.length}`}</button>
          <button class="lnk" onclick={() => (confirming = false)} disabled={busy !== ''}>Cancel</button>
        </div>
      {:else}
        <button class="btn" onclick={() => (confirming = true)} disabled={busy !== '' || !local}>Remove {removable.length}…</button>
      {/if}
    {:else}
      <p class="dim">Nothing to reclaim.</p>
    {/if}
    {#if kept.length}
      <details>
        <summary>Kept · {kept.length}</summary>
        <ul>
          {#each kept as item (item.kind + item.name)}
            <li><code>{item.name}</code> <small>{item.reason}</small></li>
          {/each}
        </ul>
      </details>
    {/if}
  {/if}

  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if note}<p class="note" role="status">{note}</p>{/if}
</Card>

<style>
  .controls { display: flex; flex-wrap: wrap; align-items: center; gap: 8px 16px; }
  .controls label { display: flex; align-items: center; gap: 6px; color: var(--dim); }
  ul { margin: 0; padding: 0; list-style: none; display: grid; gap: 4px; }
  li { display: grid; gap: 1px; background: var(--s2); border-radius: 4px; padding: 6px 10px; }
  li code { font: 12px var(--mono); overflow-wrap: anywhere; }
  li small { color: var(--dim); }
  li.failed small { color: var(--crit); }
  details summary { cursor: pointer; color: var(--ink2); }
  details ul { margin-top: 6px; }
  .confirm { display: flex; flex-wrap: wrap; align-items: center; gap: 9px; background: var(--wash-crit); border-radius: 4px; padding: 10px 11px; }
  .confirm p { flex-basis: 100%; color: var(--ink2); margin: 0; }
  .btn.d { background: var(--crit); color: var(--bg); }
  .dim, .note, .error { margin: 0; font: 12px var(--mono); }
  .dim { color: var(--dim); }
  .note { color: var(--ok); }
  .error, .blocked { color: var(--crit); }
  .blocked { margin: 0; background: var(--wash-crit); border-left: 3px solid var(--crit); padding: 8px 10px; }
</style>
