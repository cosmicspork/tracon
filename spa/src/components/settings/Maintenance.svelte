<script lang="ts">
  import { onMount, type Snippet } from 'svelte'
  import Card from './Card.svelte'
  import StorageSweep from './StorageSweep.svelte'
  import { desktopUpdatesAvailable } from '../../lib/desktop-update'
  import {
    inDesktopApp,
    installService,
    restartNode,
  } from '../../lib/desktop-setup'
  import { admin, type Maintenance as MaintenanceState } from '../../lib/admin'
  import { store } from '../../lib/store.svelte'

  let { boundary }: { boundary?: Snippet } = $props()

  let data = $state<MaintenanceState | null>(null)
  let busy = $state('')
  let error = $state('')
  let note = $state('')
  let confirmInstall = $state(false)
  let confirmUninstall = $state(false)
  let confirmRestart = $state(false)
  const desktop = inDesktopApp()
  const desktopLocal = desktop && desktopUpdatesAvailable()

  async function load() {
    try {
      data = await admin.maintenance()
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    }
  }


  async function act(action: string, work: () => Promise<string | void>) {
    if (busy) return
    busy = action
    error = ''
    note = ''
    try {
      note = (await work()) ?? ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = ''
    }
  }

  function recheckBoundary() {
    void act('boundary', async () => {
      await admin.boundaryCheck()
      await store.refetch()
      await load()
      return data?.boundary.state === 'ready' ? 'Isolation checks passed.' : 'Isolation checks have not passed; review the reported reason.'
    })
  }

  function requestRestart() {
    if (!confirmRestart) {
      confirmRestart = true
      return
    }
    void act('restart', async () => {
      if (desktopLocal && data?.local) {
        const status = await restartNode()
        await load()
        return status.service_running
          ? 'The desktop controller observed the restarted service answering.'
          : 'The desktop restart command returned, but its service status is not running.'
      }
      const result = await admin.restart()
      return result.effect
    })
  }

  function requestInstall() {
    if (!confirmInstall) {
      confirmInstall = true
      return
    }
    void act('install', async () => {
      if (desktopLocal && data?.local) {
        const status = await installService()
        await load()
        return status.service_running
          ? 'The desktop controller installed the user service and observed this node answering.'
          : 'The desktop service installation returned, but its service is not running.'
      }
      const result = await admin.install()
      return result.effect
    })
  }

  function requestUninstall() {
    if (!confirmUninstall) {
      confirmUninstall = true
      return
    }
    void act('uninstall', async () => {
      const result = await admin.uninstall()
      return result.effect
    })
  }


  const local = $derived(data?.local ?? false)
  const install = $derived(data?.service.install)
  const uninstall = $derived(data?.service.uninstall)
  const restart = $derived(data?.service.restart)

  onMount(() => {
    void load()
  })
</script>

<Card title="Runtime boundary" note="Network, gateway and images sessions run in.">
  {@render boundary?.()}
  {#if data}
    <div class="recheck">
      <p>Isolation checks: {data.boundary.state === 'ready' ? 'passed' : data.boundary.state}.</p>
      <button class="btn" onclick={recheckBoundary} disabled={busy !== '' || !local}>{busy === 'boundary' ? 'Checking boundary…' : 'Recheck runtime boundary'}</button>
      {#if data.boundary && typeof data.boundary === 'object'}
        <p class="source">This node only · <a href="/settings#mesh">compare the mesh</a></p>
      {:else}
        <p class="dim">No boundary report.</p>
      {/if}
    </div>
  {/if}
</Card>

<Card title="Service &amp; recovery" note="The user service that keeps this node running.">
  {#snippet actions()}
    <button class="lnk" onclick={() => void load()} disabled={busy !== ''}>Refresh diagnostics</button>
  {/snippet}
  {#if !data}
    <p class="dim">Unlock administrator access to see this.</p>
  {:else}
    {#if !local}
      <p class="blocked">Recovery and runtime checks need the node itself.</p>
    {/if}
    {#if !desktop}<p class="source">Service changes can disconnect this page.</p>{/if}
    <dl>
      <div><dt>Platform</dt><dd>{data.service.platform} · {data.service.supervisor}</dd></div>
      <div><dt>Unit</dt><dd>{data.service.unit_installed ? data.service.unit_path ?? 'installed at an unknown path' : 'not installed'}</dd></div>
      <div><dt>State</dt><dd class:unknown={data.service.state.state === 'unknown'}>{data.service.state.state} · {data.service.state.detail}</dd></div>
      <div><dt>Active sessions</dt><dd>{data.active_sessions}</dd></div>
      {#if data.service.container}<div><dt>Runtime</dt><dd>{data.service.container}</dd></div>{/if}
    </dl>
    {#if install}
      <div class="cap">
        <details class="capability" class:unavailable={!install.available}>
          <summary>{install.available ? 'Installation available' : 'Installation unavailable'}</summary>
          <p>{install.reason}</p>
          <small>{install.recovery}</small>
        </details>
        {#if confirmInstall}
          <div class="restart-confirm">
            <p>Install or replace the {data.service.supervisor} unit? The service restarts; active sessions are refused first.</p>
            <button class="btn d" onclick={requestInstall} disabled={busy !== '' || !local || !install.available}>{busy === 'install' ? (desktopLocal ? 'Installing and waiting…' : 'Scheduling installation…') : 'Confirm service install'}</button>
            <button class="lnk" onclick={() => (confirmInstall = false)} disabled={busy !== ''}>Cancel</button>
          </div>
        {:else}
          <button class="btn" onclick={requestInstall} disabled={busy !== '' || !local || !install.available}>{desktopLocal ? 'Install or repair desktop service…' : 'Install user service…'}</button>
        {/if}
      </div>
    {/if}

    {#if restart}
      <div class="cap">
        <details class="capability" class:unavailable={!restart.available}>
          <summary>{restart.available ? 'Restart available' : 'Restart unavailable'}</summary>
          <p>{restart.reason}</p>
          <small>{restart.recovery}</small>
        </details>
        {#if confirmRestart}
          <div class="restart-confirm">
            <p>Restart the {data.service.supervisor} service now?</p>
            <button class="btn d" onclick={requestRestart} disabled={busy !== '' || !local || !restart.available}>{busy === 'restart' ? (desktopLocal ? 'Restarting and waiting…' : 'Scheduling restart…') : 'Confirm restart of this node'}</button>
            <button class="lnk" onclick={() => (confirmRestart = false)} disabled={busy !== ''}>Cancel</button>
          </div>
        {:else}
          <button class="btn" onclick={requestRestart} disabled={busy !== '' || !local || !restart.available}>Restart this serving node…</button>
        {/if}
      </div>
    {/if}

    {#if uninstall}
      <div class="cap">
        <details class="capability" class:unavailable={!uninstall.available}>
          <summary>{uninstall.available ? 'Removal available' : 'Removal unavailable'}</summary>
          <p>{uninstall.reason}</p>
          <small>{uninstall.recovery}</small>
        </details>
        {#if confirmUninstall}
          <div class="restart-confirm">
            <p>Remove the user service? This page disconnects. State and credentials stay on disk.</p>
            <button class="btn d" onclick={requestUninstall} disabled={busy !== '' || !local || !uninstall.available}>{busy === 'uninstall' ? 'Scheduling removal…' : 'Confirm service removal'}</button>
            <button class="lnk" onclick={() => (confirmUninstall = false)} disabled={busy !== ''}>Cancel</button>
          </div>
        {:else}
          <button class="lnk d" onclick={requestUninstall} disabled={busy !== '' || !local || !uninstall.available}>Remove fixed user service…</button>
        {/if}
      </div>
    {/if}
  {/if}

  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if note}<p class="note" role="status">{note}</p>{/if}
</Card>

<StorageSweep />

<style>
  .recheck { display: grid; gap: 8px; justify-items: start; padding-top: 12px; border-top: 1px solid var(--rule); }
  .recheck p { margin: 0; color: var(--ink2); }
  dl { margin: 0; display: grid; gap: 5px; }
  dl div { display: grid; grid-template-columns: 120px minmax(0, 1fr); gap: 12px; align-items: baseline; }
  dt { color: var(--dim); font: 12px var(--mono); }
  dd { margin: 0; color: var(--ink2); font-size: 13px; overflow-wrap: anywhere; }
  dd.unknown { color: var(--wait); }
  .cap { display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: start; gap: 8px 12px; border-left: 3px solid var(--ok); border-radius: 4px; background: var(--s2); padding: 9px 12px; }
  .cap:has(.capability.unavailable) { border-left-color: var(--dim); }
  .cap > .btn, .cap > .lnk { align-self: center; }
  .cap .restart-confirm { grid-column: 1 / -1; }
  .capability { display: grid; gap: 3px; }
  .capability summary { color: var(--ink); cursor: pointer; }
  .capability p { color: var(--ink2); margin: 6px 0; }
  .capability small { color: var(--dim); }
  .restart-confirm { display: flex; flex-wrap: wrap; align-items: center; gap: 9px; background: var(--wash-crit); border-radius: 4px; padding: 10px 11px; }
  .restart-confirm p { flex-basis: 100%; color: var(--ink2); margin: 0; }
  .source, .dim, .note, .error { margin: 0; font: 12px var(--mono); }
  .source, .dim { color: var(--dim); }
  .note { color: var(--ok); }
  .error, .blocked { color: var(--crit); }
  .blocked { margin: 0; background: var(--wash-crit); border-left: 3px solid var(--crit); padding: 8px 10px; }
  @media (max-width: 700px) { dl div { grid-template-columns: 1fr; gap: 2px; } }
</style>
