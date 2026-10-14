<script lang="ts">
  // Another node's node.toml, read and edited from here over the mesh. A
  // peer answers only if its `[mesh] admin_peers` names the serving node, so
  // a peer that does not is shown with its refusal, which says why, and
  // nothing to edit. Otherwise the Configuration form edits it, offering
  // only the keys the peer lets an admin peer change; each save is confirmed
  // here (a passkey off this machine) with a prompt naming the peer and what
  // changes. `only` shows one table read-only, as Repositories shows
  // `[[repo]]`.
  import Card from './Card.svelte'
  import ConfigForm from './ConfigForm.svelte'
  import { api, ApiError } from '../../lib/api'
  import { store } from '../../lib/store.svelte'
  import type { PeerNodeConfig } from '../../lib/types'

  let { only = '' }: { only?: string } = $props()

  const peers = $derived(store.nodes.filter((node) => !node.is_self))
  let selected = $state('')
  let cfg = $state<PeerNodeConfig | null>(null)
  let refusal = $state('')
  let refusalKind = $state('')
  let busy = $state('')

  const shown = $derived(
    cfg === null || !only ? '' : JSON.stringify(cfg.document[only] ?? [], null, 2),
  )
  async function load(id: string) {
    selected = id
    cfg = null
    refusal = ''
    refusalKind = ''
    if (!id) return
    busy = 'load'
    try {
      cfg = await api.nodeConfig(id)
    } catch (cause) {
      refusal = cause instanceof Error ? cause.message : String(cause)
      refusalKind = cause instanceof ApiError ? (cause.kind ?? '') : ''
    } finally {
      busy = ''
    }
  }

</script>

<Card
  title={only ? 'A peer’s repositories' : 'A peer’s node.toml'}
  note="Read and change another node’s configuration from here. Only a peer that names this node in its [mesh] admin_peers answers; each change is confirmed here and checked again there."
>
  {#if peers.length === 0}
    <div class="empty">No peers. Pair a hub and enroll another node to administer it from here.</div>
  {:else}
    <label class="field">
      <span>Node</span>
      <select value={selected} onchange={(event) => void load(event.currentTarget.value)}>
        <option value="">Choose a peer</option>
        {#each peers as peer (peer.id)}
          <option value={peer.id}>{peer.name || peer.id.slice(0, 8)}{peer.reachable ? '' : ' (unreachable)'}</option>
        {/each}
      </select>
    </label>
    {#if busy === 'load'}
      <small>Reading…</small>
    {:else if refusal}
      <div class="empty" class:bad={refusalKind !== 'not_admin_peer'}>
        {#if refusalKind === 'not_admin_peer'}
          A peer's settings are visible and editable only from nodes it names in its
          <code>[mesh] admin_peers</code>, and that node does not name this one. Add this node's
          id there, at that node or in its own Settings with its passkey.
        {/if}
        <small>{refusal}</small>
      </div>
    {:else if cfg}
      {#if only}
        <pre class="toml">{shown}</pre>
      {:else}
        {#key selected}<ConfigForm nodeId={selected} />{/key}
      {/if}
    {/if}
  {/if}
</Card>

<style>
  .toml {
    max-height: 22rem;
    overflow: auto;
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 12px;
    font-size: 12px;
    white-space: pre-wrap;
    word-break: break-word;
  }
</style>
