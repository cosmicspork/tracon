<script lang="ts">
  // Plugins with a UI: each opens framed on its own origin (Plugin.svelte).
  // A row opens the plugin on its default channel (lib/plugin-page.ts); a
  // button beside it for each other channel it is offered on opens it there.
  // Installing and running them is Settings › Plugins.
  import PluginIcon from '../components/PluginIcon.svelte'
  import { api } from '../lib/api'
  import { pluginIconUrl } from '../lib/nav'
  import { otherChannels, pluginDefaultChannel, pluginHref } from '../lib/plugin-page'
  import { store } from '../lib/store.svelte'
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
  const nodeDefault = $derived(store.node?.default_channel)
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
      {@const first = pluginDefaultChannel(p.offered_on, nodeDefault)}
      {@const title = p.active?.ui_title || p.name}
      <div class="line">
        {#if first}
          <a class="row" href={pluginHref(p.name)} title="Open on {first}">
            <PluginIcon src={pluginIconUrl(p)} />
            <span class="t"><b>{title}</b><small>{p.name} {p.active?.version} · {p.repo} · {first}</small></span>
          </a>
          {#each otherChannels(p.offered_on, nodeDefault) as c (c)}
            <a class="arch" href={pluginHref(p.name, c)} aria-label="Open {title} on {c}">{c}</a>
          {/each}
        {:else}
          <div class="row off" aria-disabled="true">
            <PluginIcon src={pluginIconUrl(p)} />
            <span class="t"
              ><b>{title}</b><small
                >not offered on any channel · <a href="/settings#plugins">Settings › Plugins</a></small
              ></span
            >
          </div>
        {/if}
      </div>
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
  /* As an external harness line (components/ExternalAgents.svelte): the row
     takes what the side buttons leave, all of it when there are none. */
  .line {
    display: flex;
    align-items: stretch;
    gap: 4px;
    min-width: 0;
  }
  .row {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 12px;
    min-height: 52px;
    padding: 10px 14px;
    background: var(--s1);
    border-radius: 4px;
    color: var(--ink);
    text-decoration: none;
  }
  a.row:hover {
    background: var(--s2);
  }
  .row.off {
    color: var(--dim);
  }
  .t {
    display: grid;
    gap: 2px;
    min-width: 0;
  }
  .t b {
    font: 600 14.5px var(--sans);
  }
  .t small {
    color: var(--ink2);
    font: 12px var(--mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row.off b,
  .row.off small {
    color: var(--dim);
  }
  .arch {
    flex: none;
    display: flex;
    align-items: center;
    background: var(--s1);
    border-radius: 4px;
    color: var(--dim);
    font: 12.5px var(--sans);
    padding: 0 12px;
    text-decoration: none;
  }
  .arch:hover {
    color: var(--ink);
    background: var(--s2);
  }
  .note {
    color: var(--ink2);
    font-size: 12.5px;
    margin-top: 12px;
  }
</style>
