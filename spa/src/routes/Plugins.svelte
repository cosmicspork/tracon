<script lang="ts">
  // Plugins with a UI: each opens framed on its own origin (Plugin.svelte).
  // Installing and running them is Settings › Plugins.
  import { api } from '../lib/api'
  import type { PluginSummary } from '../lib/types'

  let plugins = $state<PluginSummary[] | null>(null)
  let error = $state<string | null>(null)

  $effect(() => {
    api
      .plugins()
      .then((d) => (plugins = d.plugins))
      .catch((e) => (error = e instanceof Error ? e.message : String(e)))
  })

  const withUi = $derived((plugins ?? []).filter((p) => p.state !== 'removed' && p.active?.ui))
</script>

<div class="h4">Plugins <b>{plugins ? withUi.length : '…'} with a page</b></div>

{#if error}
  <div class="banner crit">plugins <b>· {error}</b></div>
{:else if plugins === null}
  <div class="empty">Loading…</div>
{:else if withUi.length === 0}
  <div class="empty">
    No installed plugin has a page. Plugins are installed and run from
    <a href="/settings#plugins">Settings › Plugins</a>.
  </div>
{:else}
  <nav class="list" aria-label="Plugin pages">
    {#each withUi as p (p.name)}
      <a href="/plugins/{encodeURIComponent(p.name)}">
        <span><b>{p.active?.ui_title || p.name}</b><small>{p.name} {p.active?.version} · {p.repo}</small></span>
      </a>
    {/each}
  </nav>
  <p class="note">Installed, updated and restarted from <a href="/settings#plugins">Settings › Plugins</a>.</p>
{/if}

<style>
  .list {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .list a {
    display: flex;
    align-items: center;
    min-height: 52px;
    padding: 10px 14px;
    background: var(--s1);
    border-radius: 4px;
    color: var(--ink);
    text-decoration: none;
  }
  .list a:hover {
    background: var(--s2);
  }
  .list span {
    display: grid;
    gap: 2px;
    min-width: 0;
  }
  .list b {
    font: 600 14.5px var(--sans);
  }
  .list small {
    color: var(--ink2);
    font: 12px var(--mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .note {
    color: var(--ink2);
    font-size: 12.5px;
    margin-top: 12px;
  }
</style>
