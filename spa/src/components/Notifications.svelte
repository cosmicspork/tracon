<script lang="ts">
  // Pushes from this node to this browser, and which channels send them.
  // Lives on the self-node card: a subscription is a this-node, this-browser
  // fact, and the phone is exactly where the toggle matters.
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { formatAge } from '../lib/format'
  import { clock } from '../lib/clock.svelte'
  import * as push from '../lib/push'
  import { store } from '../lib/store.svelte'
  import type { PushDevice } from '../lib/types'

  let devices = $state<PushDevice[]>([])
  let on = $state(false)
  let busy = $state(false)
  let note = $state('')
  const supported = push.supported()
  const needsInstall = push.needsInstall()
  const channels = $derived(store.channels.filter((c) => c.nodes.includes(store.node?.id ?? '')))

  function notifies(bindings: Record<string, unknown>): boolean {
    const n = (bindings.notify ?? {}) as Record<string, unknown>
    if (typeof n.enabled === 'boolean') return n.enabled
    // The Phase 6 shape: a pager sink meant push, a tray sink meant not the phone.
    if (typeof n.sink === 'string') return n.sink === 'pager'
    return true
  }

  async function refresh() {
    try {
      const [sub, list] = await Promise.all([push.current(), api.pushDevices()])
      on = !!sub
      devices = list.devices
    } catch {
      devices = []
    }
  }

  async function act(f: () => Promise<unknown>, done = '') {
    busy = true
    note = ''
    try {
      await f()
      note = done
    } catch (e) {
      note = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
      await refresh()
    }
  }

  const toggle = () => act(() => (on ? push.disable() : push.enable()), on ? '' : 'On. Send a test to be sure.')
  const test = () =>
    act(async () => {
      const r = await api.testPush()
      if (!r.sent.length) throw new Error('no device registered for this login')
      const bad = r.sent.filter((s) => s.outcome !== 'Sent')
      if (bad.length) throw new Error(`push service answered ${bad.map((b) => b.outcome).join(', ')}`)
    }, 'Sent. It should have buzzed.')
  const forget = (d: PushDevice) => act(() => api.deletePushSubscription(d.id))
  const setChannel = (name: string, v: boolean) =>
    act(async () => {
      await api.putChannelBindings(name, { 'notify.enabled': v })
      await store.refetch()
    })

  onMount(() => {
    void refresh()
  })
</script>

<div class="notif">
  <div class="row">
    <span class="k">Notifications</span>
    <span class="v">
      {#if !supported}
        <span class="dim">Not available in this browser.{#if needsInstall} Add tracon to the Home Screen first.{/if}</span>
      {:else}
        <label class="tgl">
          <input type="checkbox" checked={on} disabled={busy} onchange={toggle} />
          Push to this device
        </label>
        {#if on}
          <button class="lnk" onclick={test} disabled={busy}>Send a test</button>
        {/if}
      {/if}
      {#if note}<span class="note">{note}</span>{/if}
    </span>
  </div>
  {#if channels.length}
    <div class="row">
      <span class="k">Channels</span>
      <span class="v chans">
        {#each channels as c (c.name)}
          <label class="tgl">
            <input
              type="checkbox"
              checked={notifies(c.bindings)}
              disabled={busy}
              onchange={(e) => setChannel(c.name, (e.currentTarget as HTMLInputElement).checked)}
            />
            {c.name}
          </label>
        {/each}
        <span class="dim">Every node pushes to its own devices; a channel switched off is quiet everywhere.</span>
      </span>
    </div>
  {/if}
  {#if devices.length}
    <div class="row">
      <span class="k">Devices</span>
      <span class="v devs">
        {#each devices as d (d.id)}
          <span class="dev" class:mine={d.mine}>
            <span class="ua">{d.user_agent ?? 'unknown browser'}{#if d.mine} · this browser{:else if d.local} · this machine{/if}</span>
            <span class="dim">
              {#if d.last_ok_ms}last push {formatAge(d.last_ok_ms, clock.now)} ago{:else}never pushed{/if}{#if d.fail_count}
                · {d.fail_count} failing{/if}
            </span>
            <button class="lnk d" onclick={() => forget(d)} disabled={busy}>Forget</button>
          </span>
        {/each}
      </span>
    </div>
  {/if}
</div>

<style>
  .notif {
    grid-column: 2 / -1;
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px solid var(--rule);
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .row {
    display: grid;
    grid-template-columns: 150px minmax(0, 1fr);
    gap: 0 14px;
  }
  .k {
    font-weight: 600;
    color: var(--ink);
  }
  .v {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 14px;
    min-width: 0;
  }
  .chans,
  .devs {
    flex-direction: column;
    align-items: flex-start;
    gap: 4px;
  }
  .tgl {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    cursor: pointer;
  }
  .dev {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 10px;
    align-items: baseline;
  }
  .dev.mine .ua {
    color: var(--ink);
  }
  .dim {
    color: var(--dim);
  }
  .note {
    color: var(--wait);
  }
  .lnk {
    background: none;
    border: 0;
    padding: 0;
    color: var(--acc);
    cursor: pointer;
    font: inherit;
    text-decoration: underline;
  }
  .lnk.d {
    color: var(--dim);
  }
  .lnk:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
