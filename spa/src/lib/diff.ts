/** One file's slice of a unified diff, for reading a review a file at a time. */
export interface DiffFile {
  path: string
  added: number
  removed: number
  lines: string[]
}

export type DiffKind = 'meta' | 'hunk' | 'add' | 'del' | 'ctx'

/** A diff line paired with how it should read. */
export interface DiffLine {
  text: string
  kind: DiffKind
}

/**
 * Classify diff lines by position, not by bare prefix. Inside a hunk the first
 * character is the marker, so a deleted line whose content starts with `--`
 * (a `--flag`, an SQL comment) is `del`, not a dim `---` file header — the header
 * only reads as metadata before the hunk begins. Getting this right matters:
 * the diff is the unit of review, and a mis-coloured line hides a change.
 */
export function classifyLines(lines: string[]): DiffLine[] {
  let inHunk = false
  return lines.map((text) => {
    if (text.startsWith('diff --git ')) {
      inHunk = false
      return { text, kind: 'meta' }
    }
    if (text.startsWith('@@')) {
      inHunk = true
      return { text, kind: 'hunk' }
    }
    // Before the first hunk of a file, every line is a header (`index`, `---`,
    // `+++`, `new file mode`, `rename from`, …).
    if (!inHunk) return { text, kind: 'meta' }
    if (text.startsWith('+')) return { text, kind: 'add' }
    if (text.startsWith('-')) return { text, kind: 'del' }
    // "\ No newline at end of file" and the like.
    if (text.startsWith('\\')) return { text, kind: 'meta' }
    return { text, kind: 'ctx' }
  })
}

/**
 * Split a unified diff into its files. The `+++ b/<path>` header names the file;
 * a deletion has `+++ /dev/null`, so the `--- a/<path>` side is the fallback.
 */
export function splitDiff(diff: string): DiffFile[] {
  const files: DiffFile[] = []
  let current: DiffFile | null = null
  let inHunk = false

  for (const line of diff.split('\n')) {
    if (line.startsWith('diff --git ')) {
      current = { path: line.split(' b/').pop() ?? 'file', added: 0, removed: 0, lines: [] }
      files.push(current)
      inHunk = false
    }
    if (!current) continue
    current.lines.push(line)
    if (line.startsWith('+++ b/')) {
      current.path = line.slice(6)
    } else if (line.startsWith('--- a/') && current.path === 'file') {
      current.path = line.slice(6)
    } else if (line.startsWith('@@')) {
      inHunk = true
    } else if (inHunk && line.startsWith('+')) {
      // Only count changes inside a hunk, where the first character is the
      // marker — so the `+++`/`---` file headers are never miscounted.
      current.added += 1
    } else if (inHunk && line.startsWith('-')) {
      current.removed += 1
    }
  }
  return files
}
