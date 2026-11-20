<script lang="ts">
  import { api } from '../lib/api'
  import { generatedName, nodeDisplayName } from '../lib/names'
  import { autogrow } from '../lib/autogrow'
  import { store } from '../lib/store.svelte'

  let { channel }: { channel: string } = $props()

  let candidateId = $state('')
  let destination = $state('')
  let documentSlugs = $state('')
  let memoryIds = $state('')
  let note = $state('')
  // A move takes the candidate's session over: the receiver's session holds
  // its task, project and budget, and this one closes once that import lands.
  let moving = $state(true)
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
        intent: moving ? 'move' : 'handoff',
      })
      if (response.delivery === 'portable') {
        download(response.transfer, `tracon-${generatedName('transfer', response.transfer.payload.candidate_id)}.json`)
        result = moving
          ? 'Signed package downloaded. Import it on the receiving node; this session closes once that node confirms the import over the mesh.'
          : 'Signed package downloaded. On the receiving node, choose Import a session and explicitly start the isolated continuation.'
      } else {
        result = moving
          ? `Sealed package queued for ${nodeDisplayName(store.nodes, destination)}. Once that node imports it, its new session takes this work over and this session closes.`
          : `Sealed package queued for ${nodeDisplayName(store.nodes, destination)}. That node must explicitly import it before a new isolated session starts.`
      }
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught)
    } finally {
      sending = false
    }
  }
</script>

<details class="transfer">
  <summary>Move to another node</summary>
  <p class="consequence">
    Continue a candidate elsewhere, with its branch's commits and only the named context. No credentials travel; the receiver confirms the import.
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
        <option value={node.id}>{nodeDisplayName(store.nodes, node.id)}</option>
      {/each}
    </select>
    <small>Reachable nodes in {channel}. Blank downloads a package.</small>
  </label>
  <label class="check">
    <input type="checkbox" bind:checked={moving} />
    <span>Move this work: the new session takes over its task, project and budget, and the candidate's session here closes once the receiver imports.</span>
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
    <textarea class="grow" bind:value={note} use:autogrow={note} placeholder="What the next session should know"></textarea>
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
  label.check {
    display: flex;
    gap: 8px;
    align-items: flex-start;
  }
  label.check > input { width: auto; }
  label.check > span { text-transform: none; letter-spacing: 0; }
  .transfer > .btn { margin-top: 12px; }
  .ok { color: var(--ok); }
  .transfer > p.bad { color: var(--crit); }
</style>
