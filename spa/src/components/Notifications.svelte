<script lang="ts">
  // A subscription belongs to this browser and the node serving this interface.
  // Shared channel delivery belongs in Settings > Channels, never in this device list.
  import { onMount } from 'svelte'
  import { isTauri } from '@tauri-apps/api/core'
  import { api } from '../lib/api'
  import { formatAge } from '../lib/format'
  import { clock } from '../lib/clock.svelte'
  import * as push from '../lib/push'
  import { store } from '../lib/store.svelte'
  import type { PushDevice } from '../lib/types'
  import Card from './settings/Card.svelte'


  let devices = $state<PushDevice[]>([])
  let on = $state(false)
  let busy = $state(false)
  let note = $state('')
  let testNote = $state('')
  const supported = push.supported()
  const needsInstall = push.needsInstall()
  const desktop = isTauri()
  const servingNode = $derived(store.node?.name ?? 'the serving node')

  async function refresh() {
    try {
      const [sub, list] = await Promise.all([push.current(), api.pushDevices()])
      on = !!sub
      devices = list.devices
    } catch {
      devices = []
    }
  }

  async function act(f: () => Promise<unknown>, done: string | (() => string) = '') {
    busy = true
    note = ''
    try {
      await f()
      note = typeof done === 'function' ? done() : done
    } catch (e) {
      note = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
      await refresh()
    }
  }

  const toggle = () => act(() => (on ? push.disable() : push.enable()), on ? '' : 'On. A test reports push-service acceptance, not device display.')
  const test = () =>
    act(async () => {
      const attempts = (await api.testPush()).sent
      const accepted = attempts.filter((attempt) => attempt.service_accepted)
      if (!accepted.length) {
        throw new Error(`The push service did not accept a request: ${attempts.map((attempt) => attempt.outcome).join(', ') || 'no registered devices'}`)
      }
      const refused = attempts.length - accepted.length
      testNote = `Push service accepted ${accepted.length} request${accepted.length === 1 ? '' : 's'}${refused ? `; ${refused} were not accepted` : ''}. Phone display and person receipt are not confirmed; check the device directly.`
    }, () => testNote)
  const forget = (d: PushDevice) => act(() => api.deletePushSubscription(d.id))

  onMount(() => {
    void refresh()
  })
</script>

<Card title="This device" note="Only affects this browser.">
  <div class="v">
    {#if desktop}
      <span class="dim">The desktop app uses this device’s tray. Browser push subscriptions are managed by the serving node.</span>
    {:else if !supported}
      <span class="dim">Push is not available in this browser.{#if needsInstall} On iOS, add tracon to the Home Screen first.{/if}</span>
    {:else}
      <label class="tgl">
        <input type="checkbox" checked={on} disabled={busy} onchange={toggle} />
        Push from {servingNode} to this device
      </label>
      {#if on}
        <button class="lnk" onclick={test} disabled={busy}>Send a test</button>
      {/if}
    {/if}
  </div>
  {#if note}<span class="note">{note}</span>{/if}
</Card>

<Card title="Registered devices" note={`Push subscriptions registered on ${servingNode}. Forgetting one stops only that device; shared channel settings are unchanged.`}>
  {#if devices.length}
    <div class="devs">
      {#each devices as d (d.id)}
        <div class="dev" class:mine={d.mine}>
          <span class="ua">{d.user_agent ?? 'unknown browser'}{#if d.mine} · this browser{:else if d.local} · this machine{/if}</span>
          <span class="dim">
            {#if d.last_ok_ms}service accepted {formatAge(d.last_ok_ms, clock.now)} ago{:else}no accepted push yet{/if}{#if d.fail_count}
              · {d.fail_count} failing{/if}
          </span>
          <button class="lnk d" onclick={() => forget(d)} disabled={busy}>Forget</button>
        </div>
      {/each}
    </div>
  {:else}
    <div class="empty">No devices are registered.</div>
  {/if}
</Card>

<style>
  .v {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 16px;
    min-width: 0;
  }
  .tgl {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
    color: var(--ink);
  }
  .devs {
    display: grid;
    gap: 6px;
  }
  .dev {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto;
    gap: 4px 16px;
    align-items: center;
    background: var(--s2);
    border-radius: 4px;
    padding: 9px 12px;
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .ua {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dev.mine .ua {
    color: var(--ink);
  }
  .dim {
    color: var(--dim);
  }
  .note {
    font: 12.5px var(--mono);
    color: var(--wait);
  }
  @media (max-width: 700px) {
    .dev {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
