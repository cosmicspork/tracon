<script lang="ts">
  import { api } from '../lib/api'
  import { store } from '../lib/store.svelte'

  let { channel }: { channel: string } = $props()

  let candidateId = $state('')
  let destination = $state('')
  let documentSlugs = $state('')
  let memoryIds = $state('')
  let note = $state('')
  let sending = $state(false)
  let result = $state<string | null>(null)
  let error = $state<string | null>(null)

  const destinations = $derived(
    store.nodes.filter((node) => !node.is_self && node.reachable && store.channels.some((c) => c.name === channel && c.nodes.includes(node.id))),
  )

  function ids(value: string): string[] {
    return value
      .split(',')
      .map((part) => part.trim())
      .filter(Boolean)
  }

  function download(value: unknown, name: string) {
    const blob = new Blob([JSON.stringify(value, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const link = document.createElement('a')
    link.href = url
    link.download = name
    link.click()
    URL.revokeObjectURL(url)
  }

  async function exportTransfer() {
    if (!candidateId.trim() || sending) return
    sending = true
    error = null
    result = null
    try {
      const response = await api.exportTransfer({
        candidate_id: candidateId.trim(),
        context: { documents: ids(documentSlugs), memories: ids(memoryIds), note },
        destination_node: destination || undefined,
        // A selected node means direct sealed mesh delivery. The server refuses
        // an unavailable or oversized online transfer rather than downloading
        // or importing somewhere else.
        delivery: destination ? 'mesh' : 'portable',
      })
      if (response.delivery === 'portable') {
        download(response.transfer, `tracon-transfer-${response.transfer.sha256.slice(0, 12)}.json`)
        result = 'Signed package downloaded. Import still needs confirmation on the receiving node.'
      } else {
        result = `Queued for ${destination}. That node must confirm import before a new session starts.`
      }
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      sending = false
    }
  }
</script>

<details class="transfer">
  <summary>Continue on another node</summary>
  <p>
    Export one immutable candidate and only the context named below. The source session stays running and no
    credential or live harness state is transferred.
  </p>
  <label>
    Candidate ID
    <input bind:value={candidateId} placeholder="immutable candidate id" autocapitalize="off" autocomplete="off" />
  </label>
  <label>
    Destination
    <select bind:value={destination}>
      <option value="">Portable download — choose/import offline</option>
      {#each destinations as node (node.id)}
        <option value={node.id}>{node.name || node.id.slice(0, 12)}</option>
      {/each}
    </select>
  </label>
  <label>
    Included document slugs <small>comma separated; Markdown only</small>
    <input bind:value={documentSlugs} placeholder="guide-release, ref-api" />
  </label>
  <label>
    Included memory IDs <small>comma separated</small>
    <input bind:value={memoryIds} placeholder="memory ids" />
  </label>
  <label>
    Handoff note
    <textarea bind:value={note} placeholder="What the next session should know"></textarea>
  </label>
  <button class="btn" onclick={exportTransfer} disabled={sending || !candidateId.trim()}>
    {sending ? 'Preparing…' : destination ? 'Deliver sealed package' : 'Download signed package'}
  </button>
  {#if result}<p class="ok">{result}</p>{/if}
  {#if error}<p class="bad">{error}</p>{/if}
</details>

<style>
  .transfer { margin: 14px 0; padding: 9px 11px; background: var(--s1); border-left: 3px solid var(--ink2); }
  summary { cursor: pointer; font: 600 13px var(--sans); }
  p, label { display: block; font: 12px var(--mono); color: var(--ink2); }
  label { margin-top: 8px; }
  small { color: var(--dim); }
  input, select, textarea { width: 100%; box-sizing: border-box; margin-top: 3px; }
  textarea { min-height: 48px; }
  .ok { color: var(--ok); }
  .bad { color: var(--crit); }
</style>
