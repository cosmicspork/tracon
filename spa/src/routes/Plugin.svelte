<script lang="ts">
  // One plugin's page, framed. The node serves it on a loopback origin of the
  // plugin's own, behind a capability token minted here, so its scripts run
  // with neither this interface's origin nor another plugin's; whatever it
  // asks of its API reaches the plugin as the operator's request. Popups and
  // top-level navigation stay off: the frame cannot take this page with it.
  import { api } from '../lib/api'
  import { defaultChannel, rememberChannel, rememberedChannel } from '../lib/channel'
  import { bridgeFrameTheme, currentTheme, themedFrameUrl } from '../lib/plugin-theme'
  import { store } from '../lib/store.svelte'
  import type { PluginUiOpen } from '../lib/types'

  let { name }: { name: string } = $props()

  let channel = $state('')
  let opened = $state<PluginUiOpen | null>(null)
  let error = $state<string | null>(null)
  let frame = $state<HTMLIFrameElement | null>(null)
  /** The frame's address with this page's theme, fixed when it opens: a
      later change is posted to it (lib/plugin-theme.ts), not a reload. */
  let src = $state('')

  const channels = $derived(store.channels.filter((c) => !c.archived).map((c) => c.name))

  $effect(() => {
    if (!channel && channels.length)
      channel =
        defaultChannel({ names: channels, remembered: rememberedChannel(), nodeDefault: store.node?.default_channel }) ||
        channels[0]
  })

  $effect(() => {
    const plugin = name
    const on = channel
    opened = null
    error = null
    api
      .pluginUi(plugin, on)
      .then((o) => {
        if (plugin !== name || on !== channel) return
        src = themedFrameUrl(o.url, currentTheme())
        opened = o
      })
      .catch((e) => {
        if (plugin === name && on === channel) error = e instanceof Error ? e.message : String(e)
      })
  })

  $effect(() => {
    if (frame && opened) return bridgeFrameTheme(frame, opened.url)
  })
</script>

<div class="plugin">
  <div class="h4">
    <a href="/plugins">Plugins</a>
    <b>{opened?.title ?? name}</b>
    {#if channels.length > 1}
      <label class="channel">
        <span>Channel</span>
        <select bind:value={channel} onchange={() => rememberChannel(channel)}>
          {#each channels as c (c)}<option value={c}>{c}</option>{/each}
        </select>
      </label>
    {/if}
  </div>
  {#if error}
    <div class="banner crit">{name} <b>· {error}</b></div>
  {:else if opened}
    <!-- allow-same-origin is the plugin's own origin, never this one's. -->
    <iframe
      bind:this={frame}
      title={opened.title}
      {src}
      sandbox="allow-scripts allow-same-origin allow-forms allow-downloads allow-modals"
    ></iframe>
  {:else}
    <div class="empty">Opening {name}…</div>
  {/if}
</div>

<style>
  .plugin {
    display: grid;
    grid-template-rows: auto 1fr;
    min-height: calc(100dvh - 48px);
  }
  .h4 {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .h4 a {
    text-decoration: none;
  }
  .channel {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12.5px;
    color: var(--ink2);
  }
  iframe {
    width: 100%;
    height: 100%;
    min-height: 480px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--bg);
  }
</style>
