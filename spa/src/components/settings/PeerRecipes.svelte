<script lang="ts">
  // A peer's repository recipes, administered from here: offering one of
  // its `[[repo]]` entries to its channel co-members, and accepting what it
  // was offered. The peer runs either only if its `[mesh] admin_peers` names
  // this node, and each is confirmed here with a prompt naming the peer.
  // Editing the entries themselves is A peer's repositories / node.toml.
  import Card from './Card.svelte'
  import RecipeOffers from './RecipeOffers.svelte'
  import { api } from '../../lib/api'
  import { nodeDisplayName } from '../../lib/names'
  import { peerFailure } from '../../lib/peer-admin'
  import { forgeIdentity, offerResultLine, offerTargets, type OfferTarget } from '../../lib/recipes'
  import { store } from '../../lib/store.svelte'
  import type { RecipeOfferResult } from '../../lib/types'

  const peers = $derived(store.nodes.filter((node) => !node.is_self))
  let selected = $state('')
  let paths = $state<string[] | null>(null)
  let targets = $state<OfferTarget[]>([])
  let failure = $state('')
  let busy = $state('')

  let offering = $state<string | null>(null)
  let offerChannel = $state('')
  let offerNodes = $state<string[]>([])
  let offerRepo = $state('')
  let offerNote = $state('')
  let offerResults = $state<RecipeOfferResult[]>([])
  const offerPeers = $derived(targets.find((target) => target.channel === offerChannel)?.peers ?? [])

  async function load(id: string) {
    selected = id
    paths = null
    targets = []
    failure = ''
    offering = null
    if (!id) return
    busy = 'load'
    try {
      const [environments, channels] = await Promise.all([api.repoEnvironments(id), api.channels(id)])
      if (selected !== id) return
      paths = environments.entries.map((item) => item.entry.path).filter(Boolean)
      targets = offerTargets(channels, store.nodes, id)
    } catch (cause) {
      if (selected === id) failure = peerFailure(store.nodes, id, cause)
    } finally {
      busy = ''
    }
  }

  function startOffer(path: string) {
    offering = offering === path ? null : path
    offerChannel = targets[0]?.channel ?? ''
    offerNodes = (targets[0]?.peers ?? []).map((peer) => peer.id)
    offerRepo = forgeIdentity(path) ?? ''
    offerNote = ''
    offerResults = []
  }

  function chooseChannel(channel: string) {
    offerChannel = channel
    offerNodes = (targets.find((target) => target.channel === channel)?.peers ?? []).map((peer) => peer.id)
  }

  async function sendOffer(path: string) {
    if (busy || !offerChannel || !offerNodes.length) return
    const node = selected
    busy = `offer:${path}`
    failure = ''
    try {
      const sent = await api.offerRecipe(
        {
          path,
          repo: offerRepo.trim() || undefined,
          channel: offerChannel,
          to_nodes: offerNodes,
          note: offerNote.trim() || undefined,
        },
        node,
      )
      offerResults = sent.results
    } catch (cause) {
      failure = peerFailure(store.nodes, node, cause)
    } finally {
      busy = ''
    }
  }
</script>

<Card
  title="A peer’s recipes"
  note="Offer a peer's repository recipe to its channel co-members, and accept recipes offered to it. The peer must list this node in [mesh] admin_peers."
>
  {#if peers.length === 0}
    <div class="empty">No peers.</div>
  {:else}
    <label class="field">
      <span>Node</span>
      <select value={selected} onchange={(event) => void load(event.currentTarget.value)}>
        <option value="">Choose a peer</option>
        {#each peers as peer (peer.id)}
          <option value={peer.id}>{nodeDisplayName(store.nodes, peer.id)}{peer.reachable ? '' : ' (unreachable)'}</option>
        {/each}
      </select>
    </label>
    {#if failure}<small class="bad" role="alert">{failure}</small>{/if}
    {#if busy === 'load'}
      <small>Reading…</small>
    {:else if paths}
      {#key selected}<RecipeOffers node={selected} />{/key}
      {#if paths.length === 0}
        <small>No [[repo]] entries on that node.</small>
      {/if}
      {#each paths as path (path)}
        <div class="entry">
          <span class="line">
            <code>{path}</code>
            {#if targets.length}
              <button class="lnk" type="button" onclick={() => startOffer(path)} disabled={busy !== ''}>
                {offering === path ? 'Do not offer' : 'Offer to…'}
              </button>
            {/if}
          </span>
          {#if offering === path}
            <div class="offer">
              <small>Sends its Dockerfile, preparation, checks and registries. Not its image, credentials or path.</small>
              <label>
                <span>Channel</span>
                <select value={offerChannel} onchange={(event) => chooseChannel(event.currentTarget.value)} disabled={busy !== ''}>
                  {#each targets as target (target.channel)}<option value={target.channel}>{target.channel}</option>{/each}
                </select>
              </label>
              <label>
                <span>Repository</span>
                <input bind:value={offerRepo} spellcheck="false" placeholder="from the checkout's origin" disabled={busy !== ''} />
              </label>
              <div class="to">
                <span>To</span>
                {#each offerPeers as peer (peer.id)}
                  <label class="opt-check"><input type="checkbox" bind:group={offerNodes} value={peer.id} disabled={busy !== ''} /> {peer.name}</label>
                {/each}
              </div>
              <label>
                <span>Note</span>
                <input bind:value={offerNote} placeholder="optional, shown with the offer" disabled={busy !== ''} />
              </label>
              <div class="acts">
                <button class="btn p" type="button" onclick={() => void sendOffer(path)} disabled={busy !== '' || !offerNodes.length}>
                  {busy === `offer:${path}` ? 'Offering…' : 'Offer'}
                </button>
              </div>
              {#if offerResults.length}
                <ul>
                  {#each offerResults as result (result.node)}<li class:bad={result.state === 'refused'}>{offerResultLine(result)}</li>{/each}
                </ul>
              {/if}
            </div>
          {/if}
        </div>
      {/each}
    {/if}
  {/if}
</Card>

<style>
  .entry {
    display: grid;
    gap: 6px;
  }
  .line {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 12px;
  }
  .line code {
    font: 13px var(--mono);
    overflow-wrap: anywhere;
  }
  .offer {
    display: grid;
    gap: 8px;
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 12px;
  }
  label {
    display: grid;
    gap: 5px;
    min-width: 0;
  }
  label.opt-check {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .to {
    display: grid;
    gap: 4px;
  }
  .acts {
    display: flex;
    gap: 8px;
  }
  ul {
    margin: 0;
    padding-left: 18px;
  }
  small {
    color: var(--dim);
    font-size: 12.5px;
  }
  .bad {
    color: var(--crit);
  }
</style>
