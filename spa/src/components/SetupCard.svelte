<script lang="ts">
  // Local setup is one path to a task. Home only mounts this card after it has
  // established that no ready, channel-member peer is available to compose on.
  import { setupSteps } from '../lib/firstrun'
  import { modelsForChannel } from '../lib/nodes'
  import { store } from '../lib/store.svelte'

  const steps = $derived(
    setupSteps({
      anyProviderConnected: store.providers.some((p) => p.state === 'connected'),
      modelOffered: store.channels.some((channel) => {
        const node = store.node
        return node !== null && !channel.archived && modelsForChannel(node, channel.name, store.providers, channel.bindings).length > 0
      }),
      anyChannel: store.channels.some((c) => !c.archived),
      boundaryReady: store.node?.state === 'ready' && !store.node?.harness.mismatch,
    }),
  )
  const left = $derived(steps?.filter((s) => !s.done).length ?? 0)
</script>

{#if steps}
  <div class="setup">
    <div>
      <span class="eyebrow">Run here</span>
      <p>
        Set up this node to run work locally. {left === 1 ? 'One local step remains.' : `${left} local steps remain.`}
      </p>
    </div>
    {#each steps as s, i (s.title)}
      <a href={s.href} class:done={s.done}>
        <i>{s.done ? '✓' : i + 1}</i>
        <span>{s.title}<small>{s.detail}</small></span>
      </a>
    {/each}
    <div class="existing">
      <span class="eyebrow">Use an existing setup</span>
      <p>
        Already have a node that should run this work? <a class="optional" href="/settings#mesh">Connect it to this mesh</a>.
        Once it is a ready member of an open channel, its model makes the composer available here; choose a checkout that node can access.
      </p>
    </div>
    <details>
      <summary>Optional device setup</summary>
      <p><a class="optional" href="/settings#devices">Register this browser for notifications</a> after a task path is available. It does not make a runtime or model ready.</p>
    </details>
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
    margin: 0;
    color: var(--ink2);
    font-size: 13.5px;
    line-height: 1.5;
  }
  .eyebrow {
    display: block;
    margin-bottom: 2px;
    color: var(--dim);
    font: 11px var(--mono);
    letter-spacing: 0.05em;
    text-transform: uppercase;
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
  .existing {
    display: grid;
    gap: 2px;
    margin-top: 6px;
    padding-top: 10px;
    border-top: 1px solid var(--line);
  }
  details {
    color: var(--ink2);
    font-size: 13px;
  }
  details p {
    margin-top: 4px;
  }
  a.optional {
    display: inline;
    padding: 0;
    text-decoration: underline;
    font: inherit;
  }
</style>
