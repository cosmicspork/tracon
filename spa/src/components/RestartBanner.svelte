<script lang="ts">
  // node.toml changed in a way the running node holds until it restarts, or
  // a restart is waiting: said on every screen, not just Settings, because
  // the change was made somewhere (another device, the CLI) and the restart
  // is owed here. The control is the same one Settings › System › Restart
  // has (`RestartAction`).
  import RestartAction from './RestartAction.svelte'
  import { restartBanner } from '../lib/restart'
  import { store } from '../lib/store.svelte'

  const view = $derived(restartBanner(store.restart))
</script>

{#if view}
  <div class="banner" class:crit={view.tone === 'crit'}>
    {view.headline} <b>· {view.detail}</b>
    {#if view.offer || view.withdraw}
      <RestartAction />
    {/if}
  </div>
{/if}
