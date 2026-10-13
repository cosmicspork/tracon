<script lang="ts">
  // Another node's node.toml, read and edited from here over the mesh. A
  // peer answers only if its `[mesh] admin_peers` names the serving node, so
  // a peer that does not is shown with its refusal, which says why, and
  // nothing to edit. An edit is one key at a time, confirmed here (a passkey
  // off this machine) with a prompt naming the peer and what changes; the
  // full forms are Settings' own. `only` narrows the view to one table, as
  // Repositories shows `[[repo]]`.
  import Card from './Card.svelte'
  import { api, ApiError } from '../../lib/api'
  import { store } from '../../lib/store.svelte'
  import type { ConfigOp, PeerNodeConfig } from '../../lib/types'

  let { only = '' }: { only?: string } = $props()

  const peers = $derived(store.nodes.filter((node) => !node.is_self))
  let selected = $state('')
  let cfg = $state<PeerNodeConfig | null>(null)
  let refusal = $state('')
  let refusalKind = $state('')
  let busy = $state('')
  let error = $state('')
  let note = $state('')
  let key = $state('')
  let value = $state('')

  const shown = $derived(
    cfg === null ? '' : only ? JSON.stringify(cfg.document[only] ?? [], null, 2) : cfg.toml,
  )
  const keptFromPeers = $derived(
    cfg?.keys.filter((spec) => spec.editable_by_peer === false) ?? [],
  )

  async function load(id: string) {
    selected = id
    cfg = null
    refusal = ''
    refusalKind = ''
    error = ''
    note = ''
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

  // A value is read as JSON where it is JSON (numbers, booleans, lists),
  // else kept as the text typed.
  function parsed(text: string): unknown {
    try {
      return JSON.parse(text)
    } catch {
      return text
    }
  }

  async function edit(op: ConfigOp) {
    if (!cfg) return
    busy = 'save'
    error = ''
    note = ''
    try {
      const res = await api.patchNodeConfig(selected, cfg.hash, [op])
      const said = res.changed.length
        ? `Changed ${res.changed.join(', ')}${res.restart_required ? '; a restart of that node applies it' : ''}.`
        : 'Nothing changed.'
      await load(selected)
      note = said
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
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
          Read-only and unread: that node does not name this one in its <code>[mesh] admin_peers</code>.
          Add this node's id there, at that node or in its own Settings with its passkey.
        {/if}
        <small>{refusal}</small>
      </div>
    {:else if cfg}
      <pre class="toml">{shown}</pre>
      {#if !only}
        <div class="edit">
          <label class="field">
            <span>Key</span>
            <input bind:value={key} placeholder="review.max_files" spellcheck="false" />
          </label>
          <label class="field">
            <span>Value</span>
            <input bind:value={value} placeholder="40, true, [&quot;a&quot;], or text" spellcheck="false" />
          </label>
          <div class="acts">
            <button class="btn" disabled={!key.trim() || busy !== ''} onclick={() => edit({ op: 'set', key: key.trim(), value: parsed(value) })}>
              {busy === 'save' ? 'Saving…' : 'Set'}
            </button>
            <button class="lnk d" disabled={!key.trim() || busy !== ''} onclick={() => edit({ op: 'remove', key: key.trim() })}>Remove</button>
          </div>
        </div>
        {#if keptFromPeers.length}
          <small>
            Not changed from here: {keptFromPeers.map((spec) => spec.key).join(', ')}.
            {keptFromPeers[0].why_not_by_peer ?? ''}
          </small>
        {/if}
      {/if}
      {#if note}<small class="good">{note}</small>{/if}
      {#if error}<small class="bad">{error}</small>{/if}
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
  .edit {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) auto;
    gap: 8px;
    align-items: end;
  }
  @media (max-width: 640px) {
    .edit {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .acts {
    display: flex;
    gap: 8px;
  }
</style>
