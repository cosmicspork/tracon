<script lang="ts">
  // The on-ramp, in the composer's place: shown whenever the node cannot start
  // a session, and gone the moment it can. Each step links where it is done.
  import { setupSteps } from '../lib/firstrun'
  import { store } from '../lib/store.svelte'

  const steps = $derived(
    setupSteps({
      anyProviderConnected: store.providers.some((p) => p.state === 'connected'),
      anyChannel: store.channels.some((c) => !c.archived),
      boundaryReady: store.node?.state === 'ready',
    }),
  )
  const left = $derived(steps?.filter((s) => !s.done).length ?? 0)
</script>

{#if steps}
  <div class="setup">
    <p>
      This node is a complete workspace. {left === 1 ? 'One step remains' : `${left} steps remain`} before useful work:
    </p>
    {#each steps as s, i (s.href)}
      <a href={s.href} class:done={s.done}>
        <i>{s.done ? '✓' : i + 1}</i>
        <span>{s.title}<small>{s.detail}</small></span>
      </a>
    {/each}
    <p>Remote access and mesh can be configured later in <a class="optional" href="/settings#mesh">Settings</a>.</p>
  </div>
{/if}

<style>
  .setup {
    display: flex;
    flex-direction: column;
    gap: 4px;
    background: var(--s1);
    border-radius: 6px;
    padding: 14px 16px;
    max-width: 560px;
  }
  p {
    margin: 0 0 6px;
    color: var(--ink2);
    font-size: 13.5px;
    line-height: 1.5;
  }
  a {
    display: grid;
    grid-template-columns: 22px minmax(0, 1fr);
    gap: 10px;
    align-items: baseline;
    padding: 7px 8px;
    border-radius: 3px;
    color: var(--ink);
    text-decoration: none;
    font: 500 13.5px var(--sans);
  }
  a:hover {
    background: var(--s2);
  }
  a i {
    font: 12px var(--mono);
    font-style: normal;
    color: var(--acc);
    text-align: center;
    border: 1.5px solid var(--acc);
    border-radius: 50%;
    width: 20px;
    height: 20px;
    line-height: 18px;
    box-sizing: border-box;
  }
  a.done {
    color: var(--dim);
  }
  a.done i {
    color: var(--ok);
    border-color: var(--ok);
  }
  small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    font-weight: 400;
    margin-top: 1px;
  }
  a.optional {
    display: inline;
    padding: 0;
    text-decoration: underline;
    font: inherit;
  }
</style>
