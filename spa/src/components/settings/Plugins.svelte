<script lang="ts">
  // Each installed plugin: how a call to each of its tools is answered now,
  // and the processes the node runs of it, one per channel that used it,
  // with their state and the last lines each wrote. Checking for an update
  // (which raises an update card) and rolling back are here; installing and
  // removing stay with `tracon plugin`. A peer that names this node in its
  // `[mesh] admin_peers` is administered the same way from here (`node`);
  // its processes' logs stay on it.
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import NodePicker from './NodePicker.svelte'
  import { api } from '../../lib/api'
  import { peerFailure } from '../../lib/peer-admin'
  import { store } from '../../lib/store.svelte'
  import { rollbackNote, rolledBack, updateFailure, updateNote, waitingCard, type PluginNote } from '../../lib/plugin-actions'
  import type { PluginInstance, PluginSummary } from '../../lib/types'

  /** The channels an update card can go to; none picked is the node's
      `[session] default_channel`. */
  let { channels = [] }: { channels?: string[] } = $props()

  /** The node administered: empty for this one, or a peer's id. */
  let node = $state('')
  let plugins = $state<PluginSummary[] | null>(null)
  let error = $state('')
  let busy = $state(false)
  /** Open log views, by `plugin/channel`. */
  let logs = $state<Record<string, string[] | string>>({})
  let restarting = $state<Record<string, boolean>>({})
  /** The channel update cards go to: the one being viewed, or the node's default. */
  let channel = $state('')
  /** A request in flight for a plugin, by name. */
  let acting = $state<Record<string, 'update' | 'rollback'>>({})
  /** The plugin whose rollback waits on the in-page confirmation. */
  let confirming = $state<string | null>(null)
  /** What each row says of its last update check or rollback. */
  let notes = $state<Record<string, PluginNote>>({})

  async function load() {
    busy = true
    const reading = node
    try {
      const read = (await api.plugins(reading)).plugins.filter((p) => p.state !== 'removed')
      if (reading !== node) return
      plugins = read
      error = ''
    } catch (cause) {
      if (reading !== node) return
      error = peerFailure(store.nodes, reading, cause)
    } finally {
      busy = false
    }
  }

  const key = (i: PluginInstance) => `${i.plugin}/${i.channel}`

  async function toggleLogs(i: PluginInstance) {
    const k = key(i)
    if (k in logs) {
      delete logs[k]
      return
    }
    logs[k] = []
    try {
      logs[k] = (await api.pluginLogs(i.plugin, i.channel)).lines
    } catch (cause) {
      logs[k] = cause instanceof Error ? cause.message : String(cause)
    }
  }

  async function restart(i: PluginInstance) {
    restarting[key(i)] = true
    try {
      await api.pluginRestart(i.plugin, i.channel, node)
    } catch (cause) {
      error = peerFailure(store.nodes, node, cause)
    } finally {
      restarting[key(i)] = false
      await load()
    }
  }

  async function checkUpdate(plugin: PluginSummary) {
    acting[plugin.name] = 'update'
    delete notes[plugin.name]
    try {
      const offered = await api.pluginUpdate(plugin.name, channel, node)
      notes[plugin.name] = updateNote(offered)
      // A raised card is the row's pending build now.
      if (!offered.up_to_date) await load()
    } catch (cause) {
      notes[plugin.name] = updateFailure(plugin, cause)
    } finally {
      delete acting[plugin.name]
    }
  }

  async function rollback(plugin: PluginSummary) {
    confirming = null
    acting[plugin.name] = 'rollback'
    delete notes[plugin.name]
    try {
      const now = await api.pluginRollback(plugin.name, node)
      plugins = (plugins ?? []).map((p) => (p.name === plugin.name ? rolledBack(p, now) : p))
      notes[plugin.name] = rollbackNote(now)
      // The tools and rules shown are the active build's: read them again.
      await load()
    } catch (cause) {
      notes[plugin.name] = { kind: 'error', text: peerFailure(store.nodes, node, cause) }
    } finally {
      delete acting[plugin.name]
    }
  }

  function uptime(secs: number | null): string {
    if (secs === null) return ''
    if (secs < 60) return `up ${secs} s`
    if (secs < 3600) return `up ${Math.floor(secs / 60)} min`
    return `up ${Math.floor(secs / 3600)} h`
  }

  const word = {
    starting: 'Starting',
    ready: 'Ready',
    backing_off: 'Backing off',
    unhealthy: 'Unhealthy',
    stopped: 'Stopped',
  } as const

  const answer = { allow: 'Runs', ask: 'Asked', deny: 'Refused' } as const

  function pick(next: string) {
    node = next
    plugins = null
    error = ''
    logs = {}
    notes = {}
    confirming = null
    void load()
  }

  onMount(() => void load())
</script>

<Card
  title="Plugins"
  note="Plugins installed on the node, how each tool is answered by the signed policy and its [[plugin]] entry, and a process for each channel whose sessions used one. A process stops after its idle timeout, and an unhealthy one stays down until it is used again or restarted. Check for an update (it raises an update card, and nothing is installed until the card is allowed) and roll back to the previous version here, on this node or on a peer that lists this node in [mesh] admin_peers. Install and remove with tracon plugin on the node itself."
>
  {#snippet actions()}
    <NodePicker value={node} onchange={pick} />
    {#if plugins?.length}
      <label class="to">
        Cards to
        <select bind:value={channel} aria-label="Channel update cards go to">
          <option value="">the default channel</option>
          {#each channels as c (c)}<option value={c}>{c}</option>{/each}
        </select>
      </label>
    {/if}
    <button class="btn" onclick={() => void load()} disabled={busy}>{busy ? 'Loading…' : 'Refresh'}</button>
  {/snippet}
  {#if error}
    <p class="crit">{error}</p>
  {/if}
  {#if !plugins}
    {#if !error}<p class="dim">Loading…</p>{/if}
  {:else if plugins.length === 0}
    <p class="dim">None installed. Add one with <code>tracon plugin add owner/repo</code>.</p>
  {:else}
    <ul class="plugins">
      {#each plugins as plugin (plugin.name)}
        {@const waiting = waitingCard(plugin)}
        {@const note = notes[plugin.name]}
        {@const inFlight = acting[plugin.name]}
        <li data-plugin={plugin.name}>
          <div class="line">
            <span class="name">{plugin.name}</span>
            <span class="where">{plugin.active ? plugin.active.version : 'not active'} · {plugin.repo}</span>
          </div>
          <p class="meta">
            {#if plugin.previous}previous {plugin.previous.version}{:else}no previous version{/if}
          </p>
          <div class="acts">
            {#if note?.kind !== 'refused'}
              <button class="btn small" onclick={() => void checkUpdate(plugin)} disabled={!!inFlight || !!waiting}>
                {inFlight === 'update' ? 'Checking…' : 'Check for update'}
              </button>
            {/if}
            {#if plugin.previous && plugin.active}
              <button
                class="btn small"
                onclick={() => (confirming = plugin.name)}
                disabled={!!inFlight || !!waiting || confirming === plugin.name}
              >
                {inFlight === 'rollback' ? 'Rolling back…' : `Roll back to ${plugin.previous.version}`}
              </button>
            {/if}
          </div>
          {#if confirming === plugin.name && plugin.previous && plugin.active}
            <div class="confirm" role="group" aria-label="Confirm the rollback">
              <span>
                Make {plugin.previous.version} active again in place of {plugin.active.version}? Both were approved, so no card is
                raised; its processes stop.
              </span>
              <span class="acts">
                <button class="btn small p" onclick={() => void rollback(plugin)} disabled={!!inFlight || !!waiting}>
                  Roll back to {plugin.previous.version}
                </button>
                <button class="btn small" onclick={() => (confirming = null)}>Cancel</button>
              </span>
            </div>
          {/if}
          {#if waiting}
            <p class="meta">
              An update card for {plugin.pending?.version} is waiting. <a href="/approvals/{waiting}">Open the card</a>
            </p>
          {:else if note?.kind === 'card'}
            <p class="meta">{note.text} <a href="/approvals/{note.approvalId}">Open the card</a></p>
          {/if}
          {#if note && note.kind !== 'card'}
            <p class={note.kind === 'error' || note.kind === 'refused' ? 'crit' : 'meta'} role="status">{note.text}</p>
          {/if}
          {#if plugin.tools?.length}
            <ul class="tools">
              {#each plugin.tools as tool (tool.name)}
                <li>
                  <code>{tool.name}</code>
                  <span class="chip {tool.decision}" title={tool.rule_id ?? 'no rule names it'}>{answer[tool.decision]}</span>
                </li>
              {/each}
            </ul>
          {/if}
          {#if plugin.instances.length === 0}
            <p class="meta">Not running on any channel.</p>
          {:else}
            <ul class="instances">
              {#each plugin.instances as instance (key(instance))}
                <li>
                  <div class="line">
                    <span>
                      <span class="chip {instance.state}">{word[instance.state]}</span>
                      {instance.channel}
                    </span>
                    <span class="acts">
                      {#if !node}
                        <button class="btn small" onclick={() => void toggleLogs(instance)}>
                          {key(instance) in logs ? 'Hide logs' : 'Logs'}
                        </button>
                      {/if}
                      <button class="btn small" onclick={() => void restart(instance)} disabled={restarting[key(instance)]}>
                        {restarting[key(instance)] ? 'Restarting…' : 'Restart'}
                      </button>
                    </span>
                  </div>
                  <p class="meta">
                    {#if instance.pid !== null}pid {instance.pid} · {uptime(instance.uptime_secs)} · {/if}
                    {instance.restarts} restart{instance.restarts === 1 ? '' : 's'}
                    {#if instance.backoff_ms !== null} · next try in {Math.ceil(instance.backoff_ms / 1000)} s{/if}
                  </p>
                  {#if instance.last_error}
                    <p class="crit">{instance.last_error}</p>
                  {/if}
                  {#if key(instance) in logs}
                    {@const shown = logs[key(instance)]}
                    {#if typeof shown === 'string'}
                      <p class="crit">{shown}</p>
                    {:else}
                      <pre>{shown.length ? shown.join('\n') : 'No output yet.'}</pre>
                    {/if}
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</Card>

<style>
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .plugins {
    display: grid;
    gap: 14px;
  }
  .instances {
    display: grid;
    gap: 10px;
    margin-top: 6px;
  }
  .line {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    min-width: 0;
  }
  .name {
    font-weight: 600;
  }
  .where {
    color: var(--ink2);
    overflow-wrap: anywhere;
    min-width: 0;
  }
  .acts {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .plugins > li > .acts {
    margin-top: 6px;
  }
  .confirm {
    display: grid;
    gap: 6px;
    margin-top: 6px;
    padding: 8px;
    border-radius: 4px;
    background: var(--s2);
  }
  .to {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--ink2);
    font-size: 0.9em;
  }
  .meta {
    margin: 2px 0 0;
    color: var(--ink2);
    font-size: 0.9em;
  }
  .chip {
    font-size: 0.85em;
    padding: 0 6px;
    border-radius: 4px;
    background: var(--s2);
    white-space: nowrap;
  }
  .chip.ready,
  .chip.allow {
    color: var(--ok);
  }
  .chip.unhealthy,
  .chip.deny {
    color: var(--crit);
  }
  .tools {
    display: grid;
    gap: 4px;
    margin-top: 6px;
  }
  .tools li {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    min-width: 0;
  }
  .tools code {
    overflow-wrap: anywhere;
    min-width: 0;
  }
  pre {
    margin: 6px 0 0;
    max-height: 320px;
    overflow: auto;
    font-size: 0.85em;
    background: var(--s2);
    padding: 8px;
    border-radius: 4px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .dim {
    color: var(--dim);
    margin: 0;
  }
  .crit {
    color: var(--crit);
    margin: 0;
    overflow-wrap: anywhere;
  }
</style>
