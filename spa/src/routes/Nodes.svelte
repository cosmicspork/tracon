<script lang="ts">
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import type { ProviderInfo } from '../lib/types'

  import ChannelMeters from '../components/ChannelMeters.svelte'
  import Credentials from '../components/Credentials.svelte'
  import Notifications from '../components/Notifications.svelte'
  import ProviderCard from '../components/ProviderCard.svelte'

  const nodes = $derived(store.nodes)
  const providers = $derived(store.providers)
  const reachable = $derived(nodes.filter((n) => n.is_self || n.reachable).length)
  const meshed = $derived(store.mesh !== null && store.mesh.hub.state !== 'disabled')

  // Every card starts folded: the line says whether the node is well, and the
  // providers, notifications and channels under it are opened on purpose.
  let open = $state<Record<string, boolean>>({})

  function running(id: string): number {
    return store.queue.running.filter((s) => s.node_id === id).length
  }
  function waiting(id: string): number {
    return store.queue.waiting.filter((p) => p.node_id === id).length + store.queue.reviews.filter((r) => r.node_id === id).length
  }
  function channelsOf(id: string): string[] {
    return store.channels.filter((c) => !c.archived && c.nodes.includes(id)).map((c) => c.name)
  }
  // A provider that takes only an API key, and has none, is added in Settings;
  // a card that could only say so is noise here.
  function shown(list: ProviderInfo[]): ProviderInfo[] {
    return list.filter((p) => p.can_login || p.state === 'connected')
  }
</script>

<div class="h4">
  Nodes
  <b
    >{#if meshed}{nodes.length} enrolled · {reachable} reachable{:else}this machine · no hub configured{/if}</b
  >
  {#if meshed}
    <a class="lnk r" href="/nodes/enroll">Enroll a new node</a>
  {/if}
</div>

{#if nodes.length === 0}
  <div class="empty">Waiting for the node…</div>
{:else}
  <div class="rows">
    {#each nodes as node (node.id)}
      {@const off = !node.is_self && !node.reachable}
      {@const isOpen = open[node.id] === true}
      {@const ownProviders = node.is_self ? shown(providers) : shown(node.providers ?? [])}
      <div class="node" class:bad={node.state === 'refused'} class:warn={node.harness.mismatch} class:off class:open={isOpen}>
        <span class="bar"></span>
        <button
          type="button"
          class="head"
          aria-expanded={isOpen}
          onclick={() => (open = { ...open, [node.id]: !isOpen })}
        >
          <span class="nm">
            {node.name || node.id.slice(0, 8)}
            <small>{node.is_self ? 'this node · serving you' : 'peer'} · {node.id.slice(0, 4)}…{node.id.slice(-4)}</small>
          </span>
          <span class="st">
            {#if off}
              <span class="l off">
                <span class="chip off">unreachable</span> · {node.last_seen_ms
                  ? `last seen ${formatAge(node.last_seen_ms, clock.now)}`
                  : 'never heard from'}
              </span>
            {:else if node.state === 'refused'}
              <span class="l bad"><span class="chip bad">refused</span> · {node.failed_check}</span>
            {:else if node.harness.mismatch}
              <span class="l warn"><span class="chip warn">version mismatch</span> · {node.harness.id}</span>
            {:else if node.state === 'unknown'}
              <span class="l off"><span class="chip off">not yet heard from</span></span>
            {:else}
              <span class="l">
                <span class="chip">ready</span> · {node.models.length} models · {running(node.id)} running
              </span>
            {/if}
          </span>
          <span class="tgl" aria-hidden="true">{isOpen ? '−' : '+'}</span>
        </button>
        {#if isOpen}
          <div class="body">
            <span class="detail">
              {#if off}
                {running(node.id)} session{running(node.id) === 1 ? '' : 's'} show last known state{waiting(node.id)
                  ? ` · ${waiting(node.id)} approval${waiting(node.id) === 1 ? '' : 's'} cannot be decided until it returns`
                  : ''}
              {:else if node.state === 'refused'}
                {node.failed_check}: {node.failed_detail} · still serves its interface and relays; no sessions until the check passes
              {:else if node.harness.mismatch}
                node expects {node.harness.id} {node.harness.pinned}, host has {node.harness.found} · new sessions blocked
              {:else if node.state === 'unknown'}
                admitted, no hello yet
              {:else}
                {node.harness.id} {node.harness.found ?? node.harness.pinned}{channelsOf(node.id).length
                  ? ` · ${channelsOf(node.id).join(', ')}`
                  : ''}
              {/if}
            </span>
            {#if ownProviders.length && (node.is_self || node.reachable)}
              <!-- Providers: the model credentials this node brokers. The harness
                   never holds one; connecting runs its own login there, and the
                   paste-back comes through this card. A peer's list comes from
                   its hello and the command is sealed to the owner. -->
              <div class="providers">
                {#each ownProviders as p (p.name)}
                  <ProviderCard {p} nodeId={node.id} />
                {/each}
              </div>
            {/if}
            {#if node.is_self}
              <Notifications />
            {/if}
          </div>
        {/if}
      </div>
    {/each}
  </div>
  <Credentials />
  <ChannelMeters />
{/if}

<style>
  .h4 .r {
    margin-left: auto;
    letter-spacing: 0;
    text-transform: none;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .node {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    overflow: hidden;
  }
  .bar {
    grid-row: 1 / span 2;
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--ok);
  }
  .node.bad .bar {
    background: var(--crit);
  }
  .node.bad {
    background: linear-gradient(90deg, var(--wash-crit), var(--s1) 42%);
  }
  .node.warn .bar {
    background: var(--wait);
  }
  .node.warn {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  /* Unreachable: dims, keeps its place, says when it was last seen. */
  .node.off .bar {
    background: var(--dim);
  }
  .node.off {
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 42%);
  }
  .node.off .nm,
  .node.off .st {
    color: var(--dim);
  }
  .head {
    display: grid;
    grid-template-columns: 150px minmax(0, 1fr) auto;
    gap: 0 14px;
    align-items: center;
    padding: 11px 14px 11px 0;
    background: none;
    border: 0;
    color: var(--ink);
    font: inherit;
    text-align: left;
    cursor: pointer;
    min-width: 0;
  }
  .nm {
    font-weight: 600;
    min-width: 0;
  }
  .nm small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    font-weight: 400;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .st {
    display: flex;
    flex-direction: column;
    gap: 3px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    min-width: 0;
  }
  .st span {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .st .l.bad {
    color: var(--crit);
  }
  .st .l.warn {
    color: var(--wait);
  }
  .st .l.off {
    color: var(--dim);
  }
  .tgl {
    font: 14px var(--mono);
    color: var(--dim);
  }
  .body {
    grid-column: 2;
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 0 14px 12px 0;
  }
  .detail {
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .node.bad .detail {
    color: var(--crit);
  }
  .node.warn .detail {
    color: var(--wait);
  }
  .providers {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  @media (max-width: 700px) {
    .head {
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 4px 12px;
    }
    .st {
      grid-column: 1;
    }
    .tgl {
      grid-column: 2;
      grid-row: 1 / span 2;
    }
  }
</style>
