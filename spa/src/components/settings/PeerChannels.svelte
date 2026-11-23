<script lang="ts">
  // A peer's own hold on its channels, changed from here: whether external
  // harnesses may use its broker on each channel, and whether it pushes each
  // channel's events to its devices. These bindings are each node's own (a
  // key handoff never carries them), so a member's are changed on that
  // member. The peer runs a change only if its `[mesh] admin_peers` names
  // this node, and each is confirmed here with a prompt naming the peer.
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import { nodeDisplayName } from '../../lib/names'
  import { externalStopped, notifyEnabled, peerFailure } from '../../lib/peer-admin'
  import { store } from '../../lib/store.svelte'
  import type { ChannelInfo } from '../../lib/types'

  const peers = $derived(store.nodes.filter((node) => !node.is_self))
  let selected = $state('')
  let channels = $state<ChannelInfo[] | null>(null)
  let failure = $state('')
  let busy = $state('')
  /** A change waiting on the in-page confirmation. */
  let pending = $state<{ channel: string; what: 'stop' | 'notify'; on?: boolean } | null>(null)

  const shown = $derived((channels ?? []).filter((c) => !c.archived && !c.name.startsWith('@')))

  async function load(id: string) {
    selected = id
    channels = null
    failure = ''
    pending = null
    if (!id) return
    busy = 'load'
    try {
      const read = await api.channels(id)
      if (selected === id) channels = read
    } catch (cause) {
      if (selected === id) failure = peerFailure(store.nodes, id, cause)
    } finally {
      busy = ''
    }
  }

  async function change(key: string, run: () => Promise<unknown>) {
    const node = selected
    busy = key
    failure = ''
    try {
      await run()
      pending = null
      await load(node)
    } catch (cause) {
      failure = peerFailure(store.nodes, node, cause)
    } finally {
      busy = ''
    }
  }
</script>

<Card
  title="A peer’s channels"
  note="Stop or reopen a peer's broker to external harnesses, and turn its pushes on or off, channel by channel. The peer must list this node in [mesh] admin_peers."
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
    {:else if channels && shown.length === 0}
      <small>No active channels on that node.</small>
    {:else if channels}
      <div class="rows">
        {#each shown as channel (channel.name)}
          {@const stopped = externalStopped(channel.bindings)}
          {@const on = notifyEnabled(channel.bindings ?? {})}
          <div class="row" data-channel={channel.name}>
            <span class="name">{channel.name}</span>
            <span class="acts">
              {#if stopped}
                <span class="chip bad">broker access stopped</span>
                <button
                  class="lnk"
                  disabled={busy !== ''}
                  onclick={() => change(`start:${channel.name}`, () => api.externalStart(channel.name, selected))}
                  >Start broker access</button
                >
              {:else if pending?.channel === channel.name && pending.what === 'stop'}
                <small>Refuse all external calls on {channel.name} there?</small>
                <button
                  class="btn d"
                  disabled={busy !== ''}
                  onclick={() => change(`stop:${channel.name}`, () => api.externalStop(channel.name, selected))}
                  >Stop broker access</button
                >
                <button class="lnk" disabled={busy !== ''} onclick={() => (pending = null)}>Cancel</button>
              {:else}
                <button class="lnk d" disabled={busy !== ''} onclick={() => (pending = { channel: channel.name, what: 'stop' })}
                  >Stop broker access</button
                >
              {/if}
            </span>
            <span class="acts">
              <small>pushes {on ? 'on' : 'off'}</small>
              {#if pending?.channel === channel.name && pending.what === 'notify'}
                <button
                  class="btn p"
                  disabled={busy !== ''}
                  onclick={() =>
                    change(`notify:${channel.name}`, () =>
                      api.putChannelBindings(channel.name, { 'notify.enabled': !on }, selected),
                    )}>Turn pushes {on ? 'off' : 'on'}</button
                >
                <button class="lnk" disabled={busy !== ''} onclick={() => (pending = null)}>Cancel</button>
              {:else}
                <button class="lnk" disabled={busy !== ''} onclick={() => (pending = { channel: channel.name, what: 'notify' })}
                  >Turn pushes {on ? 'off' : 'on'}</button
                >
              {/if}
            </span>
          </div>
        {/each}
      </div>
    {/if}
  {/if}
</Card>

<style>
  .rows {
    display: grid;
    gap: 8px;
  }
  .row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto;
    gap: 6px 14px;
    align-items: center;
  }
  .name {
    font: 13px var(--mono);
    overflow-wrap: anywhere;
  }
  .acts {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
  }
  small {
    color: var(--dim);
    font-size: 12.5px;
  }
  small.bad {
    color: var(--crit);
  }
  @media (max-width: 700px) {
    .row {
      grid-template-columns: 1fr;
    }
  }
</style>
