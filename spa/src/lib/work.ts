import { generatedName } from './names'
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

/** "waits on task-quiet-otter-07 Metrics rollups" — blockers by name and, when known, title. */
export function blockersLine(by: Blocker[], titles: Map<string, string>): string {
  return by
    .map((b) =>
      b.kind === 'cycle'
        ? 'part of a dependency cycle'
        : b.kind === 'unknown'
          ? `waits on ${generatedName('task', b.id)} (not seen on this node)`
          : `waits on ${generatedName('task', b.id)}${titles.has(b.id) ? ` ${titles.get(b.id)}` : ''}`,
    )
    .join(' · ')
}

/** A task's short handle: its generated name. Its title is its name. */
export function taskName(id: string): string {
  return generatedName('task', id)
}

/**
 * A project this node does not list, by the order it first appears: projects
 * are not named records, and the id is not something to read.
 */
function unlistedLabel(id: string, unlisted: readonly string[]): string {
  return unlisted.length > 1 ? `unlisted project ${unlisted.indexOf(id) + 1}` : 'unlisted project'
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
 * its title or body, ignoring case, starts its id, or is its name or part of
 * it (`task-quiet-otter-07`, `quiet-otter`, `otter`).
 */
export function matches(item: Pick<WorkItem, 'id' | 'title' | 'body'>, query: string): boolean {
  const title = item.title.toLowerCase()
  const body = item.body.toLowerCase()
  const words = query.split(/\s+/).filter(Boolean)
  const name = words.length ? taskName(item.id) : ''
  return words.every((w) => {
    const word = w.toLowerCase()
    return title.includes(word) || body.includes(word) || item.id.startsWith(word) || nameMatches(name, word)
  })
}

/** A whole name, or three or more characters of the words after `task-`: the bare kind would match all. */
function nameMatches(name: string, word: string): boolean {
  return name === word || (word.length >= 3 && name.slice('task-'.length).includes(word))
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
export function projectTabs(
  items: readonly WorkView[],
  projects: readonly Project[],
  current: ProjectKey = 'all',
): ProjectTab[] {
  const isOpen = (i: WorkView) => i.readiness.state !== 'closed'
  const open = (key: ProjectKey) => items.filter((i) => isOpen(i) && inProject(i, key)).length
  // A project with nothing open is a tab of closed work: All still shows it,
  // so it gets a tab only while it is the one open.
  const held = new Set(
    items
      .filter((i) => isOpen(i) || i.project_id === current)
      .map((i) => i.project_id)
      .filter((p): p is string => !!p),
  )
  const named = projects.filter((p) => held.has(p.id))
  const unnamed = [...held].filter((id) => !projects.some((p) => p.id === id)).sort()
  const tabs: ProjectTab[] = [{ key: 'all', label: 'All', open: open('all') }]
  for (const p of named) tabs.push({ key: p.id, label: projectLabel(p, projects), open: open(p.id) })
  for (const id of unnamed) tabs.push({ key: id, label: unlistedLabel(id, unnamed), open: open(id) })
  if (items.some((i) => !i.project_id && (isOpen(i) || current === 'none'))) {
    tabs.push({ key: 'none', label: 'No project', open: open('none') })
  }
  return tabs
}

/**
 * A project's name, told apart from another project of the same name by its
 * remote, or else by its place among them.
 */
export function projectLabel(p: Project, projects: readonly Project[]): string {
  const same = projects.filter((o) => o.name === p.name)
  if (same.length < 2) return p.name
  if (p.remote_url && same.every((o) => o.id === p.id || o.remote_url !== p.remote_url)) return `${p.name} (${p.remote_url})`
  return `${p.name} #${[...same].sort((a, b) => a.id.localeCompare(b.id)).indexOf(p) + 1}`
}

/**
 * The projects worth offering in a picker: a repository with a remote, or one
 * that already holds work. A one-off imported workspace with nothing in it is
 * not somewhere new work goes.
 */
export function pickableProjects(projects: readonly Project[], keep: string | null = null): Project[] {
  return projects.filter((p) => p.id === keep || p.total > 0 || !!p.remote_url)
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
  const p = projects.find((p) => p.id === item.project_id)
  return p ? projectLabel(p, projects) : 'unlisted project'
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
