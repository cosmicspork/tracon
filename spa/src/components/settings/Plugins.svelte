<script lang="ts">
  // Each installed plugin: how a call to each of its tools is answered now,
  // and the processes the node runs of it, one per channel that used it,
  // with their state and the last lines each wrote.
  // Installing, updating and removing stay with `tracon plugin`.
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import type { PluginInstance, PluginSummary } from '../../lib/types'

  let plugins = $state<PluginSummary[] | null>(null)
  let error = $state('')
  let busy = $state(false)
  /** Open log views, by `plugin/channel`. */
  let logs = $state<Record<string, string[] | string>>({})
  let restarting = $state<Record<string, boolean>>({})

  async function load() {
    busy = true
    try {
      plugins = (await api.plugins()).plugins.filter((p) => p.state !== 'removed')
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
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
      await api.pluginRestart(i.plugin, i.channel)
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      restarting[key(i)] = false
      await load()
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

  onMount(() => void load())
</script>

<Card
  title="Plugins"
  note="Plugins installed on this node, how each tool is answered by the signed policy and its [[plugin]] entry, and a process for each channel whose sessions used one. A process stops after its idle timeout, and an unhealthy one stays down until it is used again or restarted. Install, update and remove with tracon plugin."
>
  {#snippet actions()}
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
        <li>
          <div class="line">
            <span class="name">{plugin.name}</span>
            <span class="where">{plugin.active ? plugin.active.version : 'not active'} · {plugin.repo}</span>
          </div>
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
                      <button class="btn small" onclick={() => void toggleLogs(instance)}>
                        {key(instance) in logs ? 'Hide logs' : 'Logs'}
                      </button>
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
