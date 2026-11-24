// The navigation: the rail on a desktop, the bottom bar and More on a phone,
// all drawn from one ordered list of entries. The operator can reorder,
// hide and show entries and add plugin pages (Settings › General ›
// Navigation, and Settings › Plugins). The list is per device, in
// localStorage like the other interface preferences, and per node, so one
// browser that reaches several nodes keeps a list for each.
//
// Rules: Settings is not in the list at all; it stays pinned at the bottom
// of the rail and last under More. Home can be moved but never hidden, so
// the waiting-approvals badge is always reachable. Everything else can be
// moved, hidden and shown again. Nothing saved is today's navigation.

import type { PluginSummary } from './types'

export type BuiltIn = 'home' | 'tasks' | 'sessions' | 'docs' | 'memories' | 'plugins' | 'usage'

/** The built-in destinations, in today's order. Settings is not one of them: it is fixed. */
export const BUILT_INS: readonly BuiltIn[] = ['home', 'tasks', 'sessions', 'docs', 'memories', 'plugins', 'usage']

export type NavEntry =
  | { kind: 'builtin'; id: BuiltIn; hidden?: boolean }
  | { kind: 'plugin'; name: string; hidden?: boolean }

export const BUILT_IN_INFO: Record<BuiltIn | 'settings', { label: string; href: string; hint: string }> = {
  home: { label: 'Home', href: '/', hint: 'What is waiting on you, and what is running' },
  tasks: { label: 'Tasks', href: '/tasks', hint: 'Work on the board, from idea to done' },
  sessions: { label: 'Sessions', href: '/sessions', hint: 'Every session, running and past' },
  docs: { label: 'Documents', href: '/docs', hint: 'Documents the channels keep' },
  memories: { label: 'Memories', href: '/memories', hint: 'What sessions learned, kept for the next one' },
  plugins: { label: 'Plugins', href: '/plugins', hint: 'Pages installed plugins bring, each framed on its own origin' },
  usage: { label: 'Usage', href: '/usage', hint: 'Sessions, tokens and cost by channel' },
  settings: { label: 'Settings', href: '/settings', hint: 'Connections, channels, devices, mesh and this node' },
}

export function defaultNav(): NavEntry[] {
  return BUILT_INS.map((id) => ({ kind: 'builtin', id }))
}

/** A stable key for an entry, and for the screen it stands for. */
export function entryKey(e: NavEntry): string {
  return e.kind === 'builtin' ? e.id : `plugin:${e.name}`
}

/**
 * A stored list made whole: every built-in exactly once (a missing one is
 * put back where today's order has it, after its predecessor), Home shown,
 * plugin entries kept once each by name, and anything unreadable dropped.
 */
export function normalizeNav(stored: unknown): NavEntry[] {
  if (!Array.isArray(stored)) return defaultNav()
  const out: NavEntry[] = []
  const seen = new Set<string>()
  for (const raw of stored) {
    if (!raw || typeof raw !== 'object') continue
    const r = raw as Record<string, unknown>
    const hidden = r.hidden === true
    let e: NavEntry | null = null
    if (r.kind === 'builtin' && typeof r.id === 'string' && (BUILT_INS as readonly string[]).includes(r.id))
      e = { kind: 'builtin', id: r.id as BuiltIn, ...(hidden && r.id !== 'home' ? { hidden: true } : {}) }
    else if (r.kind === 'plugin' && typeof r.name === 'string' && r.name)
      e = { kind: 'plugin', name: r.name, ...(hidden ? { hidden: true } : {}) }
    if (!e || seen.has(entryKey(e))) continue
    seen.add(entryKey(e))
    out.push(e)
  }
  BUILT_INS.forEach((id, i) => {
    if (seen.has(id)) return
    const before = BUILT_INS.slice(0, i).reverse().find((b) => seen.has(b))
    const at = before ? out.findIndex((e) => entryKey(e) === before) + 1 : 0
    out.splice(at, 0, { kind: 'builtin', id })
    seen.add(id)
  })
  return out
}

/** Whether `list` is today's navigation. */
export function isDefaultNav(list: NavEntry[]): boolean {
  const d = defaultNav()
  return list.length === d.length && list.every((e, i) => entryKey(e) === entryKey(d[i]) && !e.hidden)
}

export function storageKey(node: string): string {
  return `tracon-nav:${node}`
}

export function loadNav(node: string, storage: Pick<Storage, 'getItem'> | null = safeStorage()): NavEntry[] {
  try {
    const raw = storage?.getItem(storageKey(node))
    return raw ? normalizeNav(JSON.parse(raw)) : defaultNav()
  } catch {
    return defaultNav()
  }
}

export function saveNav(
  node: string,
  list: NavEntry[],
  storage: Pick<Storage, 'setItem' | 'removeItem'> | null = safeStorage(),
): void {
  try {
    if (isDefaultNav(list)) storage?.removeItem(storageKey(node))
    else storage?.setItem(storageKey(node), JSON.stringify(list))
  } catch {
    /* blocked storage: the list lasts for this page */
  }
}

function safeStorage(): Storage | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage
  } catch {
    return null
  }
}

/** `list` with the entry at `index` moved by `delta` places, within bounds. */
export function moveEntry(list: NavEntry[], index: number, delta: number): NavEntry[] {
  const to = index + delta
  if (index < 0 || index >= list.length || to < 0 || to >= list.length) return list
  const out = [...list]
  const [e] = out.splice(index, 1)
  out.splice(to, 0, e)
  return out
}

/** `list` with the entry at `index` hidden or shown. Home is never hidden. */
export function setHidden(list: NavEntry[], index: number, hidden: boolean): NavEntry[] {
  const e = list[index]
  if (!e || (e.kind === 'builtin' && e.id === 'home')) return list
  return list.map((x, i) => {
    if (i !== index) return x
    const { hidden: _, ...rest } = x
    return (hidden ? { ...rest, hidden: true } : rest) as NavEntry
  })
}

export function hasPlugin(list: NavEntry[], name: string): boolean {
  return list.some((e) => e.kind === 'plugin' && e.name === name)
}

/** `list` with a plugin entry for `name` at the end, unless it has one. */
export function addPlugin(list: NavEntry[], name: string): NavEntry[] {
  return hasPlugin(list, name) ? list : [...list, { kind: 'plugin', name }]
}

export function removePlugin(list: NavEntry[], name: string): NavEntry[] {
  return list.filter((e) => !(e.kind === 'plugin' && e.name === name))
}

/** A plugin with a page that is offered on at least one channel: one the navigation can open. */
export function navigable(p: PluginSummary | undefined): boolean {
  return !!p && p.state !== 'removed' && !!p.active?.ui && (p.offered_on?.length ?? 0) > 0
}

/** Plugins with a page that `list` has no entry for: what Add can offer. */
export function addable(list: NavEntry[], plugins: PluginSummary[]): PluginSummary[] {
  return plugins.filter((p) => p.state !== 'removed' && p.active?.ui && !hasPlugin(list, p.name))
}

/** What one entry draws as. */
export type DrawnEntry = {
  key: string
  label: string
  href: string
  hint: string
  /** A built-in's id, for its icon; null for a plugin. */
  builtin: BuiltIn | null
  /** A plugin's icon address, or null for the generic plugin glyph. */
  icon: string | null
}

export function pluginIconUrl(p: PluginSummary): string | null {
  if (!p.active?.icon) return null
  return `/api/plugins/${encodeURIComponent(p.name)}/icon?v=${encodeURIComponent(p.active.digest ?? p.active.version)}`
}

/**
 * The visible entries in order, ready to draw. A plugin entry whose plugin
 * is not installed, has no page or is offered on no channel is skipped, and
 * stays in the list, so it comes back with the plugin. `plugins` null (not
 * loaded yet) skips every plugin entry.
 */
export function drawNav(list: NavEntry[], plugins: PluginSummary[] | null): DrawnEntry[] {
  const out: DrawnEntry[] = []
  for (const e of list) {
    if (e.hidden) continue
    if (e.kind === 'builtin') {
      const info = BUILT_IN_INFO[e.id]
      out.push({ key: e.id, label: info.label, href: info.href, hint: info.hint, builtin: e.id, icon: null })
      continue
    }
    const p = plugins?.find((x) => x.name === e.name)
    if (!p || !navigable(p)) continue
    out.push({
      key: entryKey(e),
      label: p.active?.ui_title || p.name,
      href: `/plugins/${encodeURIComponent(p.name)}`,
      hint: `${p.name} ${p.active?.version ?? ''}`.trim(),
      builtin: null,
      icon: pluginIconUrl(p),
    })
  }
  return out
}

/** The phone's split: the first four visible entries fill the bar; More lists the rest, then Settings. */
export const PHONE_BAR = 4

export function phoneNav(drawn: DrawnEntry[]): { bar: DrawnEntry[]; more: DrawnEntry[] } {
  return { bar: drawn.slice(0, PHONE_BAR), more: drawn.slice(PHONE_BAR) }
}

/**
 * Which entry the screen at `path` belongs to. A plugin's page is its own
 * entry's while it has one drawn, on any channel, and Plugins' otherwise.
 */
export function activeKey(path: string, drawn: DrawnEntry[]): string {
  // Enrolling a node is part of Settings › Mesh.
  if (path === '/settings' || path === '/nodes/enroll') return 'settings'
  const plugin = path.match(/^\/plugins\/([^/]+)\/?$/)
  if (plugin) {
    const key = `plugin:${decodeURIComponent(plugin[1])}`
    if (drawn.some((d) => d.key === key)) return key
    return 'plugins'
  }
  if (path.startsWith('/plugins')) return 'plugins'
  if (path.startsWith('/docs')) return 'docs'
  if (path.startsWith('/memories')) return 'memories'
  if (path.startsWith('/tasks')) return 'tasks'
  if (path.startsWith('/sessions')) return 'sessions'
  if (path === '/usage') return 'usage'
  if (path === '/more') return 'more'
  return 'home'
}
