import type { Blocker, PriorityLevel, Project, WorkItem, WorkView } from './types'

/** The row's state word, as the ledger shows it. */
export function workState(v: WorkView): 'ready' | 'blocked' | 'insession' | 'closed' {
  if (v.readiness.state === 'closed') return 'closed'
  if (v.readiness.state === 'blocked') return 'blocked'
  return v.session_id ? 'insession' : 'ready'
}

export function workLabel(v: WorkView): string {
  return { ready: 'Ready', blocked: 'Blocked', insession: 'In session', closed: 'Closed' }[workState(v)]
}

/** "waits on b07d13e8 Metrics rollups" — blockers by id and, when known, title. */
export function blockersLine(by: Blocker[], titles: Map<string, string>): string {
  return by
    .map((b) =>
      b.kind === 'cycle'
        ? 'part of a dependency cycle'
        : b.kind === 'unknown'
          ? `waits on ${b.id.slice(0, 8)} (not seen on this node)`
          : `waits on ${b.id.slice(0, 8)}${titles.has(b.id) ? ` ${titles.get(b.id)}` : ''}`,
    )
    .join(' · ')
}

export function short(id: string | null | undefined, n = 8): string {
  return id ? id.slice(0, n) : ''
}

/** A priority as people read it: its level's name, or the number when no level has it. */
export function priorityLabel(value: number, levels: readonly PriorityLevel[]): string {
  return levels.find((l) => l.value === value)?.name ?? String(value)
}

/** Whether a priority is at or above the highest named level: the ones to see first. */
export function isTopPriority(value: number, levels: readonly PriorityLevel[]): boolean {
  const top = levels.reduce<number | null>((m, l) => (m === null || l.value > m ? l.value : m), null)
  return top !== null && value >= top
}

/**
 * Whether an item answers a search, as the node's `q` does: every word is in
 * its title or body, ignoring case, or starts its id.
 */
export function matches(item: Pick<WorkItem, 'id' | 'title' | 'body'>, query: string): boolean {
  const title = item.title.toLowerCase()
  const body = item.body.toLowerCase()
  return query
    .split(/\s+/)
    .filter(Boolean)
    .every((w) => {
      const word = w.toLowerCase()
      return title.includes(word) || body.includes(word) || item.id.startsWith(word)
    })
}

/** `all`, `none` (items with no project), or a project's id. */
export type ProjectKey = string

export interface ProjectTab {
  key: ProjectKey
  label: string
  /** Open items in it. */
  open: number
}

/**
 * The tabs the Tasks page offers: everything, then each project that holds an
 * item, by name, then the items with no project. A project this node has no
 * name for (an item from a peer) is shown by its id.
 */
export function projectTabs(items: readonly WorkView[], projects: readonly Project[]): ProjectTab[] {
  const open = (key: ProjectKey) => items.filter((i) => i.readiness.state !== 'closed' && inProject(i, key)).length
  const held = new Set(items.map((i) => i.project_id).filter((p): p is string => !!p))
  const named = projects.filter((p) => held.has(p.id))
  const unnamed = [...held].filter((id) => !projects.some((p) => p.id === id)).sort()
  const tabs: ProjectTab[] = [{ key: 'all', label: 'All', open: open('all') }]
  for (const p of named) tabs.push({ key: p.id, label: p.name, open: open(p.id) })
  for (const id of unnamed) tabs.push({ key: id, label: short(id), open: open(id) })
  if (items.some((i) => !i.project_id)) tabs.push({ key: 'none', label: 'No project', open: open('none') })
  return tabs
}

/** Whether an item is under a tab. */
export function inProject(item: Pick<WorkItem, 'project_id'>, key: ProjectKey): boolean {
  if (key === 'all') return true
  if (key === 'none') return !item.project_id
  return item.project_id === key
}

/** The name an item's project goes by here: its name, its short id, or null for none. */
export function projectName(item: Pick<WorkItem, 'project_id'>, projects: readonly Project[]): string | null {
  if (!item.project_id) return null
  return projects.find((p) => p.id === item.project_id)?.name ?? short(item.project_id)
}

const TAB_KEY = 'tracon.tasks.project.'

/** The tab last open on a channel, if the browser kept one. */
export function rememberedProject(channel: string): ProjectKey | null {
  try {
    return localStorage.getItem(TAB_KEY + channel)
  } catch {
    return null
  }
}

export function rememberProject(channel: string, key: ProjectKey) {
  try {
    if (key === 'all') localStorage.removeItem(TAB_KEY + channel)
    else localStorage.setItem(TAB_KEY + channel, key)
  } catch {
    // A private window or blocked storage: the tab is simply not remembered.
  }
}
