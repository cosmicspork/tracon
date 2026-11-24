// Which channel a plugin's page opens on. A plugin runs per channel, and only
// on the channels it is offered on (`offered_on`), so the choice is among
// those: the address's `?channel=`, else the node's default channel when the
// plugin is offered there, else the first it is offered on. A channel in the
// address that the plugin is not offered on is an error, never a silent
// fall back to another.

import type { PluginSummary } from './types'

/** The channel a plugin opens on with none named, or null when it is offered on none. */
export function pluginDefaultChannel(offered: string[] | null | undefined, nodeDefault?: string | null): string | null {
  const on = offered ?? []
  if (nodeDefault && on.includes(nodeDefault)) return nodeDefault
  return on[0] ?? null
}

/** The channels besides the default one, each a button beside the plugin's row. */
export function otherChannels(offered: string[] | null | undefined, nodeDefault?: string | null): string[] {
  const first = pluginDefaultChannel(offered, nodeDefault)
  return (offered ?? []).filter((c) => c !== first)
}

/** A plugin page's address, on `channel` when one is named. */
export function pluginHref(name: string, channel?: string | null): string {
  const base = `/plugins/${encodeURIComponent(name)}`
  return channel ? `${base}?channel=${encodeURIComponent(channel)}` : base
}

export type PageChannel = { channel: string } | { error: string }

/** The channel the page at `search` (`?channel=`) opens `plugin` on, or why it cannot open. */
export function pageChannel(search: string, plugin: PluginSummary | undefined, nodeDefault?: string | null): PageChannel {
  if (!plugin || plugin.state === 'removed') return { error: 'not installed on this node' }
  if (!plugin.active?.ui) return { error: 'has no page' }
  const offered = plugin.offered_on ?? []
  const named = new URLSearchParams(search).get('channel')
  if (named) {
    if (offered.includes(named)) return { channel: named }
    return { error: `is not offered on channel ${named}` }
  }
  const first = pluginDefaultChannel(offered, nodeDefault)
  return first ? { channel: first } : { error: 'is not offered on any channel' }
}

/** The browser tab's title while a plugin's page is open. */
export function pluginTabTitle(title: string, channel: string): string {
  return `${title} · ${channel}`
}
