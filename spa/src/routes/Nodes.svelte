<script lang="ts">
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { store } from '../lib/store.svelte'
  import { surface } from '../lib/surface.svelte'

  import ChannelMeters from '../components/ChannelMeters.svelte'
  import { api } from '../lib/api'
  import type { ProviderInfo } from '../lib/types'

  const nodes = $derived(store.nodes)
  const providers = $derived(store.providers)
  let codes = $state<Record<string, string>>({})
  let busy = $state<Record<string, boolean>>({})
  let errors = $state<Record<string, string>>({})

  async function act(p: ProviderInfo, f: () => Promise<unknown>) {
    busy = { ...busy, [p.name]: true }
    errors = { ...errors, [p.name]: '' }
    try {
      await f()
    } catch (e) {
      errors = { ...errors, [p.name]: e instanceof Error ? e.message : String(e) }
    } finally {
      busy = { ...busy, [p.name]: false }
    }
  }
  function connect(p: ProviderInfo) {
    return act(p, () => api.connectProvider(p.name, ['personal']))
  }
  function paste(p: ProviderInfo) {
    const code = (codes[p.name] ?? '').trim()
    if (!code) return
    return act(p, async () => {
      await api.providerCode(p.name, code)
      codes = { ...codes, [p.name]: '' }
    })
  }
  function disconnect(p: ProviderInfo) {
    return act(p, () => api.disconnectProvider(p.name))
  }
  function expiry(p: ProviderInfo): string {
    if (!p.expires_ms) return ''
    const left = p.expires_ms - clock.now
    return left <= 0 ? 'expired' : `refreshes in ${formatAge(clock.now - left, clock.now)}`
  }
  const reachable = $derived(nodes.filter((n) => n.is_self || n.reachable).length)
  const meshed = $derived(store.mesh !== null && store.mesh.hub.state !== 'disabled')

  function running(id: string): number {
    return store.queue.running.filter((s) => s.node_id === id).length
  }
  function waiting(id: string): number {
    return store.queue.waiting.filter((p) => p.node_id === id).length + store.queue.reviews.filter((r) => r.node_id === id).length
  }
</script>

<div class="h4">
  Nodes
  <b
    >{#if meshed}{nodes.length} enrolled · {reachable} reachable{:else}this machine · no hub configured{/if}</b
  >
  {#if meshed && !surface.phone}
    <a class="lnk r" href="/nodes/enroll">Enroll a new node</a>
  {/if}
</div>

{#if nodes.length === 0}
  <div class="empty">Waiting for the node…</div>
{:else}
  <div class="rows">
    {#each nodes as node (node.id)}
      {@const off = !node.is_self && !node.reachable}
      <div class="node" class:bad={node.state === 'refused'} class:warn={node.harness.mismatch} class:off>
        <span class="bar"></span>
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
            <span
              >{running(node.id)} session{running(node.id) === 1 ? '' : 's'} show last known state{waiting(node.id)
                ? ` · ${waiting(node.id)} approval${waiting(node.id) === 1 ? '' : 's'} cannot be decided until it returns`
                : ''}</span
            >
          {:else if node.state === 'refused'}
            <span class="l bad">
              <span class="chip bad">refused</span> · {node.failed_check}: {node.failed_detail}
            </span>
            <span>Still serves its interface and relays. No sessions until the check passes.</span>
          {:else if node.harness.mismatch}
            <span class="l warn">
              <span class="chip warn">version mismatch</span> · node expects {node.harness.id}
              {node.harness.pinned}, host has {node.harness.found} · new sessions blocked
            </span>
          {:else if node.state === 'unknown'}
            <span class="l off"><span class="chip off">not yet heard from</span> · admitted, no hello yet</span>
          {:else}
            <span class="l">
              <span class="chip">ready</span> · boundary check passed · {node.harness.id}
              {node.harness.found ?? node.harness.pinned}
            </span>
            <span
              >{node.models.length} models offered · {running(node.id)} session{running(node.id) === 1 ? '' : 's'} running{store
                .channels.filter((c) => c.nodes.includes(node.id)).length
                ? ` · ${store.channels
                    .filter((c) => c.nodes.includes(node.id))
                    .map((c) => c.name)
                    .join(', ')}`
                : ''}</span
            >
          {/if}
        </span>
      </div>
      {#if node.is_self && providers.length}
        <!-- Providers: the model credentials this node brokers. The harness
             never holds one; connecting runs its own login here, and the
             paste-back comes through this card. -->
        <div class="providers">
          {#each providers as p (p.name)}
            <div class="prov" class:pending={p.state === 'pending'} class:bad={p.state === 'failed'}>
              <span class="pbar"></span>
              <span class="pnm">
                {p.name}
                <small
                  >{#if p.state === 'connected'}{p.kind === 'oauth' ? 'subscription' : 'api key'}{#if p.identity}
                      · {p.identity}{/if}{:else if p.state === 'pending'}waiting on you{:else if p.state === 'failed'}failed{:else}not
                    connected{/if}</small
                >
              </span>
              <span class="pst">
                {#if p.state === 'connected'}
                  <span class="l"><span class="chip">connected</span>{#if p.channels.length} · {p.channels.join(', ')}{/if}{#if expiry(p)} · {expiry(p)}{/if}</span>
                  {#if !surface.phone}
                    <span><button class="lnk d" onclick={() => disconnect(p)} disabled={busy[p.name]}>Disconnect</button></span>
                  {/if}
                {:else if p.state === 'pending'}
                  <span class="l warn"><span class="chip warn">connect</span> · open the link, sign in, paste what it gives you back</span>
                  <span><a class="lnk" href={p.url} target="_blank" rel="noopener">Open {p.name} sign-in</a></span>
                  {#if !surface.phone}
                    <span class="paste">
                      <input
                        placeholder="redirect URL or code"
                        bind:value={codes[p.name]}
                        onkeydown={(e) => e.key === 'Enter' && paste(p)}
                      />
                      <button class="btn p" onclick={() => paste(p)} disabled={busy[p.name] || !(codes[p.name] ?? '').trim()}>Paste back</button>
                      <button class="lnk d" onclick={() => disconnect(p)} disabled={busy[p.name]}>Cancel</button>
                    </span>
                  {/if}
                {:else}
                  <span class="l off" class:bad={p.state === 'failed'}
                    ><span class="chip" class:off={p.state !== 'failed'} class:bad={p.state === 'failed'}>{p.state === 'failed' ? 'failed' : 'disconnected'}</span>{#if p.error} · {p.error}{/if}</span
                  >
                  {#if p.can_login && !surface.phone}
                    <span><button class="lnk" onclick={() => connect(p)} disabled={busy[p.name]}>{p.state === 'failed' ? 'Try again' : 'Connect'}</button></span>
                  {:else if !p.can_login}
                    <span>API key only: <code>tracon credential import</code> on this node.</span>
                  {/if}
                {/if}
                {#if errors[p.name]}<span class="l bad">{errors[p.name]}</span>{/if}
              </span>
            </div>
          {/each}
        </div>
      {/if}
    {/each}
  </div>
  {#if meshed && surface.phone}
    <div class="empty">Enrolling a node needs a desktop browser.</div>
  {/if}
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
    grid-template-columns: 3px 150px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    padding: 11px 14px 11px 0;
    overflow: hidden;
  }
  .bar {
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
  /* Providers sit under the serving node, indented past its bar. */
  .providers {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin: 0 0 6px 17px;
  }
  .prov {
    display: grid;
    grid-template-columns: 3px 133px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    padding: 9px 14px 9px 0;
    overflow: hidden;
  }
  .pbar {
    align-self: stretch;
    border-radius: 2px 0 0 2px;
    background: var(--ok);
  }
  .prov.pending .pbar {
    background: var(--wait);
  }
  .prov.pending {
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
  }
  .prov.bad .pbar {
    background: var(--crit);
  }
  .pnm {
    font-weight: 600;
    min-width: 0;
  }
  .pnm small {
    display: block;
    font: 11.5px var(--mono);
    color: var(--dim);
    font-weight: 400;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pst {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    min-width: 0;
  }
  .pst > span {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pst .l.warn {
    color: var(--wait);
  }
  .pst .l.off {
    color: var(--dim);
  }
  .pst .l.bad {
    color: var(--crit);
  }
  .paste {
    display: flex;
    gap: 8px;
    align-items: center;
    white-space: normal;
  }
  .paste input {
    flex: 1;
    min-width: 0;
    font: 12.5px var(--mono);
    background: var(--s2);
    color: var(--ink);
    border: 0;
    border-radius: 3px;
    padding: 6px 8px;
  }
  .pst code {
    font: 12px var(--mono);
    color: var(--ink);
  }
  @media (max-width: 700px) {
    .node {
      grid-template-columns: 3px minmax(0, 1fr);
      gap: 4px 12px;
    }
    .st {
      grid-column: 2;
    }
  }
</style>
