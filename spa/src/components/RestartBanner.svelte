<script lang="ts">
  // node.toml changed in a way the running node holds until it restarts, or
  // a restart is waiting: said on every screen, not just Settings, because
  // the change was made somewhere (another device, the CLI) and the restart
  // is owed here. Restarting is an administrator's action: off the node the
  // passkey prompt (`StepUpPrompt`) confirms it.
  import { api } from '../lib/api'
  import { restartBanner } from '../lib/restart'
  import { store } from '../lib/store.svelte'

  const view = $derived(restartBanner(store.restart))
  let confirming = $state(false)
  let busy = $state(false)
  let error = $state('')

  async function act(run: () => Promise<unknown>) {
    busy = true
    error = ''
    try {
      await run()
      confirming = false
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
      await store.refreshRestart()
    }
  }

  function restart(mode: 'when_idle' | 'now') {
    const hash = store.restart?.config?.hash
    if (!hash) return
    return act(() => api.restart(mode, hash, mode === 'now'))
  }
</script>

{#if view}
  <div class="banner" class:crit={view.tone === 'crit'}>
    {view.headline} <b>· {view.detail}</b>
    {#if view.offer}
      <button class="lnk" type="button" disabled={busy} onclick={() => restart('when_idle')}>Restart when idle</button>
      <button class="lnk" type="button" disabled={busy} onclick={() => (confirming = !confirming)}>Restart now…</button>
    {/if}
    {#if view.withdraw}
      <button class="lnk" type="button" disabled={busy} onclick={() => act(() => api.cancelRestart())}>Don't restart</button>
    {/if}
    {#if confirming && view.offer}
      {@const closing = store.restart?.closing ?? []}
      <i>
        {#if closing.length}
          Restarting now ends {closing.length} live session{closing.length === 1 ? '' : 's'}{closing.some((s) => s.turn_active) ? ', cutting off turns that are running' : ''}:
          {#each closing as s (s.id)}<span class="session">{s.branch || s.id} on {s.channel}{s.turn_active ? ' (working)' : ''}</span>{/each}
        {:else}
          No session is live; restarting now ends nothing.
        {/if}
        <button class="lnk" type="button" disabled={busy} onclick={() => restart('now')}
          >{closing.length ? `End ${closing.length} and restart` : 'Restart'}</button
        >
      </i>
    {/if}
    {#if error}<i class="err">{error}</i>{/if}
  </div>
{/if}

<style>
  .session {
    display: block;
    overflow-wrap: anywhere;
  }
  .err {
    color: var(--crit);
  }
</style>
