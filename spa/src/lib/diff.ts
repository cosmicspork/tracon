/** One file's slice of a unified diff, for reading a review a file at a time. */
export interface DiffFile {
  path: string
  added: number
  removed: number
  lines: string[]
}

/**
 * Split a unified diff into its files. The `+++ b/<path>` header names the file;
 * a deletion has `+++ /dev/null`, so the `--- a/<path>` side is the fallback.
 */
export function splitDiff(diff: string): DiffFile[] {
  const files: DiffFile[] = []
  let current: DiffFile | null = null

  for (const line of diff.split('\n')) {
    if (line.startsWith('diff --git ')) {
      current = { path: line.split(' b/').pop() ?? 'file', added: 0, removed: 0, lines: [] }
      files.push(current)
    }
    if (!current) continue
    current.lines.push(line)
    if (line.startsWith('+++ b/')) {
      current.path = line.slice(6)
    } else if (line.startsWith('--- a/') && current.path === 'file') {
      current.path = line.slice(6)
    } else if (line.startsWith('+') && !line.startsWith('+++')) {
      current.added += 1
    } else if (line.startsWith('-') && !line.startsWith('---')) {
      current.removed += 1
    }
  }
  return files
}
