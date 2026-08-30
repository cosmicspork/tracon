<script lang="ts">
  // The on-ramp: shown in place of the queue's empty state until the first
  // session exists, then never again. Each step links where it is done.
  import { api } from '../lib/api'
  import { firstRunSteps } from '../lib/firstrun'
  import { store } from '../lib/store.svelte'

  let anyReady = $state(false)
  $effect(() => {
    void store.workVersion
    Promise.all(
      store.channels.map((c) =>
        api
          .workReady(c.name)
          .then((d) => d.items.length > 0)
          .catch(() => false),
      ),
    ).then((flags) => (anyReady = flags.some(Boolean)))
  })

  const steps = $derived(
    firstRunSteps({
      anyProviderConnected: store.providers.some((p) => p.state === 'connected'),
      anyReadyWork: anyReady,
      anySession: store.sessions.size > 0,
    }),
  )
</script>

{#if steps}
  <div class="firstrun">
    <p>Nothing is waiting on you yet. Three steps stand between this node and its first session:</p>
    {#each steps as s, i (s.href)}
      <a href={s.href} class:done={s.done}>
        <i>{s.done ? '✓' : i + 1}</i>
        <span>{s.title} <small>{s.detail}</small></span>
      </a>
    {/each}
  </div>
{/if}

<style>
  .firstrun {
    display: flex;
    flex-direction: column;
    gap: 4px;
    background: var(--s1);
    border-radius: 4px;
    padding: 14px 16px;
    max-width: 520px;
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
</style>
