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
  import { Banner, Empty, Row, SectionHeader, SideButton } from '@tracon/ui/svelte'

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

<SectionHeader detail="{plugins ? withUi.length : '…'} with a page">Plugins</SectionHeader>

{#if error}
  <Banner tone="crit" detail={error}>plugins</Banner>
{:else if plugins === null}
  <Empty loading>Loading…</Empty>
{:else if withUi.length === 0}
  <Empty>
    No installed plugin has a page. Plugins are installed and run from
    <a href="/settings#plugins">Settings › Plugins</a>.
  </Empty>
{:else}
  <nav class="list" aria-label="Plugin pages">
    {#each withUi as p (p.name)}
      {@const first = pluginDefaultChannel(p.offered_on, nodeDefault)}
      {@const title = p.active?.ui_title || p.name}
      {#if first}
        <Row href={pluginHref(p.name)} title="Open on {first}" minHeight={52}>
          {#snippet lead()}<PluginIcon src={pluginIconUrl(p)} />{/snippet}
          <span class="t"><b>{title}</b><small>{p.name} {p.active?.version} · {p.repo} · {first}</small></span>
          {#snippet actions()}
            {#each otherChannels(p.offered_on, nodeDefault) as c (c)}
              <SideButton href={pluginHref(p.name, c)} label="Open {title} on {c}">{c}</SideButton>
            {/each}
          {/snippet}
        </Row>
      {:else}
        <Row off minHeight={52}>
          {#snippet lead()}<PluginIcon src={pluginIconUrl(p)} />{/snippet}
          <span class="t off"
            ><b>{title}</b><small
              >not offered on any channel · <a href="/settings#plugins">Settings › Plugins</a></small
            ></span
          >
        </Row>
      {/if}
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
  .t.off b,
  .t.off small {
    color: var(--dim);
  }
  .note {
    color: var(--ink2);
    font-size: 12.5px;
    margin-top: 12px;
  }
</style>
