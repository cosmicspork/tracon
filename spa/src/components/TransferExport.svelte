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
        result = 'Signed package downloaded. On the receiving node, choose Import a session and explicitly start the isolated continuation.'
      } else {
        result = `Sealed package queued for ${destination}. That node must explicitly import it before a new isolated session starts.`
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
  <p class="consequence">
    Continue one immutable candidate with only the named context. The source session stays unchanged; credentials and a
    live harness are never transferred. The receiving node must confirm import, which creates a new isolated workspace.
  </p>
  <label>
    <span>Source candidate ID</span>
    <input bind:value={candidateId} placeholder="Immutable candidate ID" autocapitalize="off" autocomplete="off" />
  </label>
  <label>
    <span>Destination node</span>
    <select bind:value={destination}>
      <option value="">Portable download — choose and import on another node</option>
      {#each destinations as node (node.id)}
        <option value={node.id}>{node.name || node.id.slice(0, 12)}</option>
      {/each}
    </select>
    <small>Only reachable nodes in {channel} are shown. Leave this blank to download a portable package.</small>
  </label>
  <label>
    <span>Included document slugs</span>
    <small>Comma separated; Markdown only.</small>
    <input bind:value={documentSlugs} placeholder="guide-release, ref-api" />
  </label>
  <label>
    <span>Included memory IDs</span>
    <small>Comma separated.</small>
    <input bind:value={memoryIds} placeholder="Memory IDs" />
  </label>
  <label>
    <span>Handoff note</span>
    <textarea bind:value={note} placeholder="What the next session should know"></textarea>
  </label>
  <button class="btn" type="button" onclick={exportTransfer} disabled={sending || !candidateId.trim()}>
    {sending ? 'Preparing package…' : destination ? 'Send sealed package' : 'Download signed package'}
  </button>
  {#if result}<p class="ok">{result}</p>{/if}
  {#if error}<p class="bad">{error}</p>{/if}
</details>

<style>
  .transfer {
    margin: 14px 0;
    padding: 9px 11px;
    background: var(--s1);
    border-left: 3px solid var(--ink2);
  }
  summary { cursor: pointer; font: 600 13px var(--sans); }
  .consequence, label, .transfer > p {
    color: var(--ink2);
    font: 12px var(--mono);
  }
  .consequence { margin: 4px 0 10px; }
  label {
    display: grid;
    gap: 3px;
    margin-top: 10px;
  }
  label > span {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink2);
  }
  small { color: var(--dim); }
  input, select, textarea { width: 100%; }
  textarea { min-height: 72px; resize: vertical; }
  .transfer > .btn { margin-top: 12px; }
  .ok { color: var(--ok); }
  .bad { color: var(--crit); }
</style>
