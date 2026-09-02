<script lang="ts">
  // The on-ramp, in the composer's place: shown whenever the node cannot start
  // a session, and gone the moment it can. Each step links where it is done.
  import { setupSteps } from '../lib/firstrun'
  import { store } from '../lib/store.svelte'

  const steps = $derived(
    setupSteps({
      anyProviderConnected: store.providers.some((p) => p.state === 'connected'),
      anyChannel: store.channels.some((c) => !c.archived),
      hubPaired: store.mesh?.hub.state === 'connected',
    }),
  )
  const left = $derived(steps?.filter((s) => !s.done && !s.optional).length ?? 0)
</script>

{#if steps}
  <div class="setup">
    <p>
      {store.node?.state === 'refused'
        ? 'The boundary is refusing, and it is the first thing to fix.'
        : 'The boundary holds.'}
      {left === 1 ? 'One step stands' : `${left} steps stand`} between this node and a session it can start:
    </p>
    {#each steps as s, i (s.href)}
      <a href={s.href} class:done={s.done}>
        <i>{s.done ? '✓' : i + 1}</i>
        <span
          >{s.title}{#if s.optional}<em>optional</em>{/if}
          <small>{s.detail}</small></span
        >
      </a>
    {/each}
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
  a em {
    font: 11px var(--mono);
    font-style: normal;
    color: var(--dim);
    margin-left: 8px;
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
</style>
