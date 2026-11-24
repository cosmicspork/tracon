<script lang="ts">
  // One plugin's page, framed, and nothing else: no heading, no channel
  // picker, no padding, so the page has the whole of main. The channel comes
  // from the address (`?channel=`), else the node's default channel when the
  // plugin is offered there, else the first it is offered on
  // (lib/plugin-page.ts); the tab title says which is open.
  //
  // The node serves the page on a loopback origin of the plugin's own, behind
  // a capability token minted here, so its scripts run with neither this
  // interface's origin nor another plugin's; whatever it asks of its API
  // reaches the plugin as the operator's request. Popups and top-level
  // navigation stay off: the frame cannot take this page with it.
  import { api } from '../lib/api'
  import { navigation } from '../lib/navigation.svelte'
  import { pageChannel, pluginTabTitle } from '../lib/plugin-page'
  import { bridgeFrameTheme, currentTheme, themedFrameUrl } from '../lib/plugin-theme'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import type { PluginUiOpen } from '../lib/types'

  let { name }: { name: string } = $props()

  let opened = $state<PluginUiOpen | null>(null)
  let error = $state<string | null>(null)
  let frame = $state<HTMLIFrameElement | null>(null)
  /** The frame's address with this page's theme, fixed when it opens: a
      later change is posted to it (lib/plugin-theme.ts), not a reload. */
  let src = $state('')
  /** Whether the frame has loaded. Until it has, it stays hidden over this
      page's background: WKWebView paints a framed page white until its first
      styled paint, which on a cold load is long enough to flash. */
  let loaded = $state(false)

  // What the node says of its plugins now, not what it said when the
  // navigation first read them.
  $effect(() => {
    void name
    void navigation.refreshPlugins()
  })

  const summary = $derived(navigation.plugins?.find((p) => p.name === name))
  const title = $derived(summary?.active?.ui_title || name)
  const choice = $derived(
    navigation.plugins === null ? null : pageChannel(router.search, summary, store.node?.default_channel),
  )
  const channel = $derived(choice && 'channel' in choice ? choice.channel : '')

  $effect(() => {
    const plugin = name
    const on = channel
    opened = null
    error = null
    loaded = false
    if (!on) return
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
    if (!channel) return
    const before = document.title
    document.title = pluginTabTitle(opened?.title ?? title, channel)
    return () => {
      document.title = before
    }
  })

  $effect(() => {
    if (frame && opened) return bridgeFrameTheme(frame, opened.url)
  })

  const problem = $derived(choice && 'error' in choice ? choice.error : error)
</script>

<div class="plugin">
  {#if problem}
    <div class="state" role="alert">
      <div class="banner crit">{title} <b>· {problem}</b></div>
      <a class="lnk" href="/plugins">All plugins</a>
    </div>
  {:else if opened}
    <!-- allow-same-origin is the plugin's own origin, never this one's. -->
    <iframe
      bind:this={frame}
      title={opened.title}
      {src}
      class:loaded
      onload={() => (loaded = true)}
      sandbox="allow-scripts allow-same-origin allow-forms allow-downloads allow-modals"
    ></iframe>
  {:else}
    <div class="state"><div class="empty">Opening {title}…</div></div>
  {/if}
</div>

<style>
  /* main gives this screen no padding (App.svelte, .full), so the frame
     fills everything beside the rail, or above the phone's tab bar. */
  .plugin {
    display: grid;
    height: calc(100dvh - var(--tabbar));
    min-height: 0;
  }
  iframe {
    display: block;
    width: 100%;
    height: 100%;
    border: 0;
    border-radius: 0;
    background: var(--bg);
    visibility: hidden;
  }
  iframe.loaded {
    visibility: visible;
  }
  .state {
    display: grid;
    place-content: center;
    justify-items: center;
    gap: 12px;
    padding: 24px;
    text-align: center;
  }
</style>
