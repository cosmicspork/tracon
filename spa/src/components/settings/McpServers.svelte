<script lang="ts">
  // Each `[[mcp_server]]` as the node sees it: whether it could be listed,
  // and how a call to each of its tools is answered now. The entries
  // themselves are edited in Configuration, where every node.toml table is.
  import { onMount } from 'svelte'
  import Card from './Card.svelte'
  import { api } from '../../lib/api'
  import type { McpServerStatus } from '../../lib/types'

  let servers = $state<McpServerStatus[] | null>(null)
  let error = $state('')
  let busy = $state(false)

  async function load(refresh = false) {
    busy = true
    try {
      servers = (await api.mcpServers(refresh)).servers
      error = ''
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause)
    } finally {
      busy = false
    }
  }

  function age(secs: number | null): string {
    if (secs === null) return ''
    if (secs < 60) return 'just now'
    if (secs < 3600) return `${Math.floor(secs / 60)} min ago`
    return `${Math.floor(secs / 3600)} h ago`
  }

  const word = { allow: 'Runs', ask: 'Asked', deny: 'Refused' } as const

  onMount(() => void load())
</script>

<Card
  title="MCP servers"
  note="Other MCP servers whose tools sessions reach through this node, as <server>__<tool>. Only the node speaks to them. Each tool runs, is asked or is refused by the allow, ask and deny lists of its entry, under a signed policy denial."
>
  {#snippet actions()}
    <button class="btn" onclick={() => void load(true)} disabled={busy}>{busy ? 'Listing…' : 'List again'}</button>
  {/snippet}
  {#if error}
    <p class="crit">{error}</p>
  {:else if !servers}
    <p class="dim">Listing…</p>
  {:else if servers.length === 0}
    <p class="dim">None registered. Add an <code>mcp_server</code> entry under <a href="#config">Configuration</a>.</p>
  {:else}
    <ul class="servers">
      {#each servers as server (server.name)}
        <li>
          <div class="line">
            <span class="name">{server.name}</span>
            <span class="where">{server.url}</span>
          </div>
          <p class="meta">
            {server.channels.length ? `Channels: ${server.channels.join(', ')}` : 'Serves no channel'}
            {#if server.credential} · credential {server.credential}{/if}
            · <a href="#config">Edit</a>
          </p>
          {#if server.error}
            <p class="crit">{server.listed ? 'Last listing failed: ' : ''}{server.error}</p>
          {/if}
          {#if server.listed}
            <p class="meta">{server.tools.length} tool{server.tools.length === 1 ? '' : 's'}, listed {age(server.listed_secs_ago)}</p>
            <ul class="tools">
              {#each server.tools as tool (tool.name)}
                <li title={tool.description}>
                  <code>{tool.remote}</code>
                  <span class="chip {tool.decision}" title={tool.rule_id ?? 'no rule names it'}>{word[tool.decision]}</span>
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
  .servers {
    display: grid;
    gap: 14px;
  }
  .line {
    display: flex;
    justify-content: space-between;
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
  .meta {
    margin: 2px 0 0;
    color: var(--ink2);
    font-size: 0.9em;
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
  .chip {
    font-size: 0.85em;
    padding: 0 6px;
    border-radius: 4px;
    background: var(--s2);
    white-space: nowrap;
  }
  .chip.allow {
    color: var(--ok);
  }
  .chip.deny {
    color: var(--crit);
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
