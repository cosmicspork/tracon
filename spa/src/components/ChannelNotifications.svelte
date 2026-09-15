<script lang="ts">
  // A channel binding is shared with its members; it is never a per-device filter.
  import { api } from '../lib/api'
  import { store } from '../lib/store.svelte'
  import Card from './settings/Card.svelte'

  type Delivery = Awaited<ReturnType<typeof api.putChannelBindings>>['delivery']

  let pending = $state<{ name: string; enabled: boolean } | null>(null)
  let busy = $state(false)
  let failure = $state('')
  let result = $state<Record<string, Delivery>>({})

  const channels = $derived(store.channels.filter((channel) => !channel.archived))

  function enabled(bindings: Record<string, unknown>): boolean {
    const notify = (bindings.notify ?? {}) as Record<string, unknown>
    if (typeof notify.enabled === 'boolean') return notify.enabled
    // The former bridge configuration used a tray sink to mean no push.
    return typeof notify.sink === 'string' ? notify.sink !== 'tray' : true
  }

  function nodeNames(ids: string[]): string {
    return ids
      .map((id) => store.nodes.find((node) => node.id === id)?.name ?? id.slice(0, 8))
      .join(', ')
  }

  async function save() {
    if (!pending || busy) return
    const change = pending
    busy = true
    failure = ''
    try {
      const response = await api.putChannelBindings(change.name, { 'notify.enabled': change.enabled })
      const delivery = response.delivery
      result = {
        ...result,
        [change.name]: delivery,
      }
      pending = null
      await store.refetch()
    } catch (caught) {
      failure = caught instanceof Error ? caught.message : String(caught)
    } finally {
      busy = false
    }
  }
</script>

<Card title="Shared notifications" note="Whether a channel's member nodes push its events to their registered devices. Shared with every member; this browser's own subscription is under Devices.">
  {#if failure}<small class="delivery bad" role="alert">{failure}</small>{/if}
  {#if channels.length === 0}
    <small>No active channels yet.</small>
  {:else}
    <div class="channels">
      {#each channels as channel (channel.name)}
        {@const on = enabled(channel.bindings)}
        {@const members = nodeNames(channel.nodes)}
        {@const delivery = result[channel.name]}
        <div class="channel">
          <span class="name">
            {channel.name}
            <small>{on ? 'shared notifications on' : 'shared notifications off'} · affects {channel.nodes.length} node{channel.nodes.length === 1 ? '' : 's'}{members ? `: ${members}` : ''}</small>
          </span>
          <span class="action">
            {#if pending?.name === channel.name}
              <span class="confirm" role="status">
                {pending.enabled ? 'Enable' : 'Disable'} notifications for every listed node’s registered
                devices? This changes the shared channel, not a device subscription.
                <button class="btn p" onclick={save} disabled={busy}>{busy ? 'Saving…' : 'Confirm change'}</button>
                <button class="lnk" onclick={() => (pending = null)} disabled={busy}>Cancel</button>
              </span>
            {:else}
              <button class="lnk" onclick={() => (pending = { name: channel.name, enabled: !on })} disabled={busy}>
                Turn shared notifications {on ? 'off' : 'on'}
              </button>
            {/if}
          </span>
          {#if delivery}
            <small class:bad={delivery.state === 'failed'} class="delivery">
              {#if delivery.state === 'local'}
                Saved on the serving node only; no member propagation was reported.
              {:else if delivery.state === 'queued'}
                {#if delivery.handed_to !== null}
                  Saved on the serving node and queued for {delivery.handed_to} node{delivery.handed_to === 1 ? '' : 's'}; it is not applied there until that node receives it.
                {:else}
                  Saved on the serving node and queued for member nodes; it is not applied there until each node receives it.
                {/if}
              {:else}
                Saved on the serving node; distribution is incomplete, and some peers may already have received this change{delivery.error ? `: ${delivery.error}` : ''}.
              {/if}
            </small>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</Card>

<style>
  small {
    margin: 0;
    color: var(--dim);
    font-size: 12.5px;
  }
  .channels {
    display: grid;
    gap: 6px;
  }
  .channel {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 4px 14px;
    align-items: start;
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 12px;
  }
  .name {
    color: var(--ink);
    font-weight: 500;
    min-width: 0;
  }
  .name small,
  .delivery {
    display: block;
    font: 11.5px var(--mono);
    margin-top: 3px;
  }
  .action {
    text-align: right;
  }
  .confirm {
    display: flex;
    max-width: 420px;
    flex-wrap: wrap;
    justify-content: flex-end;
    align-items: center;
    gap: 6px 10px;
    color: var(--ink2);
    font: 11.5px var(--mono);
  }
  .delivery {
    grid-column: 1 / -1;
    color: var(--wait);
  }
  .delivery.bad {
    color: var(--crit);
  }
  @media (max-width: 700px) {
    .channel {
      grid-template-columns: minmax(0, 1fr);
    }
    .action {
      text-align: left;
    }
    .confirm {
      justify-content: flex-start;
    }
  }
</style>
