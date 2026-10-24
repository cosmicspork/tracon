// Where an address that used to name a screen now leads, and which screen a
// current one opens. Bookmarks, links in documents and notifications already
// delivered keep working after a screen moves; the router swaps the address in
// place rather than adding a step to history. Pure, so it tests flat.

/**
 * Settings sections that were folded into others when Settings was regrouped,
 * by the anchor they had. Maintenance was a catch-all; most of it is System.
 */
export const SETTINGS_ALIASES: Record<string, string> = {
  repositories: 'channels',
  devices: 'access',
  data: 'system',
  maintenance: 'system',
}

/**
 * The current address for `path` (with its `search` and `hash`), or null when
 * it is already current.
 */
export function redirect(path: string, search = '', hash = ''): string | null {
  // Usage was Metrics before it had a rail entry of its own.
  if (path === '/metrics') return `/usage${search}${hash}`
  // Work was a group of tabs; its first tab, the task list, is Tasks now.
  if (path === '/work') return `/tasks${search}${hash}`
  const item = path.match(/^\/work\/([^/]+)$/)
  if (item) return `/tasks/${item[1]}${search}${hash}`
  // Nodes left the rail: its facts sit in Settings › Mesh beside the members.
  if (path === '/nodes') return `/settings${search}#mesh`
  // A section was only ever a hash, but a path is the obvious thing to type.
  const section = path.match(/^\/settings\/([a-z]+)\/?$/)
  if (section) return `/settings${search}#${SETTINGS_ALIASES[section[1]] ?? section[1]}`
  if (path === '/settings') {
    const moved = SETTINGS_ALIASES[hash.slice(1)]
    if (moved) return `/settings${search}#${moved}`
  }
  return null
}

/** Settings' sections, by the anchor that opens each, in the order its nav lists them. */
export const SETTINGS_SECTIONS = [
  ['general', 'General'],
  ['connections', 'Connections'],
  ['channels', 'Channels & repositories'],
  ['access', 'Access & devices'],
  ['policies', 'Permissions & policies'],
  ['mesh', 'Mesh'],
  ['config', 'Configuration'],
  ['system', 'System'],
] as const

export type SettingsSection = (typeof SETTINGS_SECTIONS)[number][0]

/** The section `hash` opens: General for none, or for one Settings does not have. */
export function settingsSection(hash: string): SettingsSection {
  return SETTINGS_SECTIONS.find(([id]) => id === hash.slice(1))?.[0] ?? 'general'
}

/**
 * The screen App.svelte renders at a current address (see `redirect`). The
 * login gate and a device approval replace whatever this says: they hang on
 * the node's answer and on the fragment, not on the path.
 */
export type Screen =
  | { name: 'doc-preview'; channel: string; slug: string }
  | { name: 'review' | 'approval' | 'promotion' | 'session' | 'task'; id: string }
  | { name: 'doc'; channel: string; slug: string; edit: boolean }
  | { name: 'sessions' | 'enroll' | 'usage' | 'tasks' | 'more' | 'evidence' | 'docs' | 'memories' | 'settings' | 'home' }

/** Every screen's name, so a test can hold the screen audit to all of them. */
export const SCREENS = Object.keys({
  'doc-preview': 1,
  review: 1,
  approval: 1,
  promotion: 1,
  session: 1,
  sessions: 1,
  enroll: 1,
  usage: 1,
  task: 1,
  tasks: 1,
  more: 1,
  evidence: 1,
  doc: 1,
  docs: 1,
  memories: 1,
  settings: 1,
  home: 1,
} satisfies Record<Screen['name'], 1>) as Screen['name'][]

export function screenFor(path: string): Screen {
  // First match wins: a session's address also starts with /sessions.
  const preview = path.match(/^\/docs\/([^/]+)\/([^/]+)\/preview$/)
  if (preview) return { name: 'doc-preview', channel: preview[1], slug: preview[2] }
  for (const [prefix, name] of [
    ['reviews', 'review'],
    ['approvals', 'approval'],
    ['promotions', 'promotion'],
    ['sessions', 'session'],
  ] as const) {
    const m = path.match(new RegExp(`^/${prefix}/([^/]+)`))
    if (m) return { name, id: m[1] }
  }
  if (path === '/sessions') return { name: 'sessions' }
  if (path === '/nodes/enroll') return { name: 'enroll' }
  if (path === '/usage') return { name: 'usage' }
  const task = path.match(/^\/tasks\/([^/]+)/)
  if (task) return { name: 'task', id: task[1] }
  if (path === '/tasks') return { name: 'tasks' }
  if (path === '/more') return { name: 'more' }
  if (path === '/evidence') return { name: 'evidence' }
  const doc = path.match(/^\/docs\/([^/]+)\/([^/]+)(\/edit)?$/)
  if (doc) return { name: 'doc', channel: doc[1], slug: doc[2], edit: Boolean(doc[3]) }
  if (path.startsWith('/docs')) return { name: 'docs' }
  if (path.startsWith('/memories')) return { name: 'memories' }
  if (path === '/settings') return { name: 'settings' }
  return { name: 'home' }
}

/** How the Usage page's breakdown keys its rows, past channel and provider. */
export type UsageGroup = 'model' | 'day' | 'session'

/**
 * What the Usage page shows, as its address carries it: `?since=` is a span
 * of days back from today (7, 30, 90, or any other count) or a first day
 * `YYYY-MM-DD`; `?until=` a last day, inclusive; `?provider=` and `?group=`
 * narrow and key the breakdown. Anything unreadable falls back to the
 * default rather than to an empty page.
 */
export interface UsageView {
  /** Days back from today; null when `from` sets the start. */
  days: number | null
  from: string | null
  to: string | null
  provider: string | null
  group: UsageGroup
}

export const USAGE_DEFAULT_DAYS = 30
const DATE = /^\d{4}-\d{2}-\d{2}$/

function validDate(s: string | null): string | null {
  if (!s || !DATE.test(s)) return null
  const d = new Date(`${s}T00:00:00`)
  return Number.isNaN(d.getTime()) || localDate(d.getTime()) !== s ? null : s
}

/** `YYYY-MM-DD` of the browser's local day holding `ms`. */
export function localDate(ms: number): string {
  const d = new Date(ms)
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
}

export function usageView(search: string): UsageView {
  const q = new URLSearchParams(search)
  const since = q.get('since') ?? ''
  const from = validDate(since)
  const n = /^\d+$/.test(since) ? Number(since) : 0
  const group = q.get('group')
  const until = validDate(q.get('until'))
  // An address whose start is after its end reads as the same days, swapped.
  const swap = from !== null && until !== null && from > until
  return {
    days: from ? null : n > 0 ? n : USAGE_DEFAULT_DAYS,
    from: swap ? until : from,
    to: swap ? from : until,
    provider: q.get('provider')?.trim() || null,
    group: group === 'day' || group === 'session' ? group : 'model',
  }
}

/** The query for `view`, leaving out what is already the default. */
export function usageSearch(view: UsageView): string {
  const q = new URLSearchParams()
  if (view.from) q.set('since', view.from)
  else if (view.days !== null && view.days !== USAGE_DEFAULT_DAYS) q.set('since', String(view.days))
  if (view.to) q.set('until', view.to)
  if (view.provider) q.set('provider', view.provider)
  if (view.group !== 'model') q.set('group', view.group)
  const s = q.toString()
  return s ? `?${s}` : ''
}

/**
 * The window `view` covers, in epoch ms: `[since, until)`, local days in the
 * browser's zone. `until` is null when the window runs to now.
 */
export function usageWindow(view: UsageView, now = Date.now()): { since: number; until: number | null } {
  const dayStart = (s: string) => new Date(`${s}T00:00:00`)
  // The day after the last: a calendar step, so a DST change keeps midnight.
  const end = view.to ? dayStart(view.to) : null
  end?.setDate(end.getDate() + 1)
  const until = end ? end.getTime() : null
  const since = view.from ? dayStart(view.from).getTime() : (until ?? now) - (view.days ?? USAGE_DEFAULT_DAYS) * 86_400_000
  return { since, until }
}
