// What to call a repository in a list. A managed clone lives at
// `<root>/<host>/<owner>/<name>/repo`, so the last segment is the word "repo"
// for every one of them; the segment above it is the name a person recognises.

/** The label for a repository path, preferring the forge's own full name. */
export function repoLabel(path: string, fullName?: string | null): string {
  if (fullName && fullName.trim() !== '') return fullName
  const parts = path.split('/').filter(Boolean)
  if (parts.length === 0) return path
  const last = parts[parts.length - 1]
  if (last === 'repo' && parts.length > 1) return parts[parts.length - 2]
  return last
}

/** Whether a repository row answers a search: every word of the query appears in its label, name or host. */
export function repoMatches(query: string, ...fields: (string | null | undefined)[]): boolean {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean)
  if (words.length === 0) return true
  const hay = fields.filter(Boolean).join(' ').toLowerCase()
  return words.every((w) => hay.includes(w))
}

/**
 * Whether a path is a checkout the node made for itself. Those live under the
 * node's own root and are named by the forge, so the path is nothing a person
 * needs to read; one typed by the operator is the operator's own to see.
 */
export function isManagedPath(path: string, managed: { repo_path: string }[]): boolean {
  return managed.some((m) => m.repo_path === path)
}

/**
 * A forge repository's identity, `host/owner/name`: what a new session names
 * its repository by. The node resolves it to its own clone, cloning it at the
 * start when it has none yet.
 */
export function repoIdentity(repo: { host: string; full_name: string }): string {
  return `${repo.host}/${repo.full_name}`
}

/**
 * The managed clone a repository choice names: by its identity, or by the
 * clone's path, which is how a session's row records it.
 */
export function managedFor<T extends { repo_path: string; host: string; full_name: string }>(
  value: string,
  managed: T[],
): T | null {
  return managed.find((m) => m.repo_path === value || repoIdentity(m) === value) ?? null
}

/**
 * The repository a new session here starts in by default, by its identity:
 * the newest one this node and channel ran in that is still one of the node's
 * own clones. A path typed for an earlier session may be gone since, and the
 * picker would not offer it, so it is never chosen for the operator.
 */
export function defaultRepo(
  sessions: Iterable<{ node_id: string; channel: string; repo_path: string; created_ms: number }>,
  nodeId: string,
  channel: string,
  managed: { repo_path: string; host: string; full_name: string }[],
): string | null {
  const last = [...sessions]
    .filter((s) => s.node_id === nodeId && s.channel === channel && isManagedPath(s.repo_path, managed))
    .sort((a, b) => b.created_ms - a.created_ms)[0]
  const clone = last ? managedFor(last.repo_path, managed) : null
  return clone ? repoIdentity(clone) : null
}

/**
 * The repository a work item's project names, by identity, when it has one:
 * what a session on that item starts in, ahead of whatever the channel used
 * last. A project recorded from an imported workspace names none.
 */
export function projectRepo(
  projects: { id: string; remote_url: string | null }[],
  projectId: string | null,
): string | null {
  if (!projectId) return null
  const url = projects.find((p) => p.id === projectId)?.remote_url?.trim()
  return url ? url : null
}
