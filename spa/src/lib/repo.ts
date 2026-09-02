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
