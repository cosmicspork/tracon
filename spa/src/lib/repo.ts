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
