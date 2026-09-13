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

<div class="notif">
  <div class="row">
    <span class="k">This device</span>
    <span class="v">
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
      {#if note}<span class="note">{note}</span>{/if}
    </span>
  </div>
  <div class="row">
      <span class="k">Registered devices</span>
      <span class="v devs">
        <small>These subscriptions are registered on {servingNode}. Forgetting one stops only that device’s subscription; it does not change any shared channel.</small>
        {#each devices as d (d.id)}
          <span class="dev" class:mine={d.mine}>
            <span class="ua">{d.user_agent ?? 'unknown browser'}{#if d.mine} · this browser{:else if d.local} · this machine{/if}</span>
            <span class="dim">
              {#if d.last_ok_ms}service accepted {formatAge(d.last_ok_ms, clock.now)} ago{:else}no accepted push yet{/if}{#if d.fail_count}
                · {d.fail_count} failing{/if}
            </span>
            <button class="lnk d" onclick={() => forget(d)} disabled={busy}>Forget</button>
          </span>
        {/each}
      </span>
    </div>
</div>

<style>
  .notif {
    display: flex;
    flex-direction: column;
    gap: 10px;
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
  @media (max-width: 700px) {
    .row {
      grid-template-columns: minmax(0, 1fr);
      gap: 4px;
    }
  }
</style>
