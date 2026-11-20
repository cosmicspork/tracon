<script lang="ts">
  // Settings › System › Restart: restart this node whenever it can be
  // restarted, not only to apply a node.toml change. It says how the node
  // restarts and what a restart applies, and has the same single Restart
  // control as the banner (`RestartAction`). Not gated on being at the node:
  // off it, the passkey prompt confirms the restart, and a refusal is shown
  // inline.
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import RestartAction from '../RestartAction.svelte'
  import { EXIT_MECHANISM, restartAsk, restartCard } from '../../lib/restart'
  import { store } from '../../lib/store.svelte'

  const status = $derived(store.restart)
  const view = $derived(status ? restartCard(status) : null)
  const ask = $derived(status ? restartAsk(status) : null)

  onMount(() => {
    void store.refreshRestart()
  })
</script>

<Card title="Restart" note="Restart this node: running turns are waited for or ended as you choose, and this page reconnects when it is back.">
  {#if !status || !view}
    <p class="dim">Reading how this node restarts…</p>
  {:else}
    <dl>
      <div>
        <dt>Restarts by</dt>
        <dd>{view.mechanism ?? 'nothing: this node cannot restart itself'}</dd>
      </div>
      <div><dt>Applies</dt><dd>{view.applies}</dd></div>
      {#if view.plan}<div><dt>Waiting</dt><dd>{view.plan}</dd></div>{/if}
      {#if view.lastError}<div><dt>Last restart</dt><dd class="crit">did not happen: {view.lastError}.</dd></div>{/if}
    </dl>
    {#if view.mechanism === EXIT_MECHANISM}
      <p class="source">The node drains, exits, and its container is started again in place (a Kubernetes pod keeps its volume and address); this page reconnects.</p>
    {/if}
    {#if ask?.kind === 'unavailable'}
      <p class="blocked">Restart unavailable: {ask.reason}.</p>
    {/if}
    <div class="act"><RestartAction variant="card" /></div>
  {/if}
</Card>

<style>
  dl { margin: 0; display: grid; gap: 5px; }
  dl div { display: grid; grid-template-columns: 120px minmax(0, 1fr); gap: 12px; align-items: baseline; }
  dt { color: var(--dim); font: 12px var(--mono); }
  dd { margin: 0; color: var(--ink2); font-size: 13px; overflow-wrap: anywhere; }
  dd.crit { color: var(--crit); }
  .act { display: flex; flex-wrap: wrap; align-items: center; gap: 8px 10px; }
  .source, .dim { margin: 0; font: 12px var(--mono); color: var(--dim); }
  .blocked { margin: 0; color: var(--crit); background: var(--wash-crit); border-left: 3px solid var(--crit); padding: 8px 10px; overflow-wrap: anywhere; }
  @media (max-width: 700px) { dl div { grid-template-columns: 1fr; gap: 2px; } }
</style>
