// What the operator changed in a call's arguments, shown before they allow it.

export interface LineChange {
  kind: 'same' | 'add' | 'del'
  text: string
}

export interface ArgumentChange {
  key: string
  kind: 'added' | 'removed' | 'changed'
  lines: LineChange[]
}

/**
 * A line-by-line diff of `before` against `after`, by longest common
 * subsequence. Empty text has no lines: against it, the other side is all
 * added or all removed, as a new document is.
 */
export function diffLines(before: string, after: string): LineChange[] {
  if (before === after && before === '') return []
  if (before === '') return after.split('\n').map((text) => ({ kind: 'add', text }))
  if (after === '') return before.split('\n').map((text) => ({ kind: 'del', text }))
  const a = before.split('\n')
  const b = after.split('\n')
  let start = 0
  while (start < a.length && start < b.length && a[start] === b[start]) start++
  let endA = a.length
  let endB = b.length
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--
    endB--
  }

  const n = endA - start
  const m = endB - start
  // lcs[i][j]: the common subsequence length of a[start+i..endA) and b[start+j..endB).
  const lcs = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1))
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i]![j] =
        a[start + i] === b[start + j] ? lcs[i + 1]![j + 1]! + 1 : Math.max(lcs[i + 1]![j]!, lcs[i]![j + 1]!)
    }
  }

  const out: LineChange[] = a.slice(0, start).map((text) => ({ kind: 'same', text }))
  let i = 0
  let j = 0
  while (i < n || j < m) {
    if (i < n && j < m && a[start + i] === b[start + j]) {
      out.push({ kind: 'same', text: a[start + i]! })
      i++
      j++
    } else if (j < m && (i === n || lcs[i]![j + 1]! >= lcs[i + 1]![j]!)) {
      out.push({ kind: 'add', text: b[start + j]! })
      j++
    } else {
      out.push({ kind: 'del', text: a[start + i]! })
      i++
    }
  }
  for (const text of a.slice(endA)) out.push({ kind: 'same', text })
  return reorder(out)
}

// Within each run of changes, show every removed line before the lines that
// replace it, the way a unified diff does.
function reorder(lines: LineChange[]): LineChange[] {
  const out: LineChange[] = []
  let dels: LineChange[] = []
  let adds: LineChange[] = []
  const flush = () => {
    out.push(...dels, ...adds)
    dels = []
    adds = []
  }
  for (const line of lines) {
    if (line.kind === 'del') dels.push(line)
    else if (line.kind === 'add') adds.push(line)
    else {
      flush()
      out.push(line)
    }
  }
  flush()
  return out
}

/** A run of lines to show, or unchanged lines folded away between them. */
export type Hunk = { kind: 'lines'; lines: LineChange[] } | { kind: 'skip'; count: number }

/**
 * A diff with long unchanged runs folded: each change keeps `context`
 * unchanged lines either side, and what lies beyond is counted, not shown.
 * A diff with no changes folds to a single skip.
 */
export function hunks(lines: LineChange[], context = 3): Hunk[] {
  const out: Hunk[] = []
  const push = (line: LineChange) => {
    const last = out[out.length - 1]
    if (last?.kind === 'lines') last.lines.push(line)
    else out.push({ kind: 'lines', lines: [line] })
  }
  let i = 0
  while (i < lines.length) {
    if (lines[i]!.kind !== 'same') {
      push(lines[i]!)
      i++
      continue
    }
    let end = i
    while (end < lines.length && lines[end]!.kind === 'same') end++
    // Context is kept only on a side that has a change.
    const head = i === 0 ? 0 : context
    const tail = end === lines.length ? 0 : context
    if (end - i > head + tail) {
      for (const line of lines.slice(i, i + head)) push(line)
      out.push({ kind: 'skip', count: end - i - head - tail })
      for (const line of lines.slice(end - tail, end)) push(line)
    } else {
      for (const line of lines.slice(i, end)) push(line)
    }
    i = end
  }
  return out
}

function asText(value: unknown): string {
  return typeof value === 'string' ? value : JSON.stringify(value, null, 2)
}

/** The arguments that differ between `original` and `edited`, each as a line diff. */
export function diffArguments(original: Record<string, unknown>, edited: Record<string, unknown>): ArgumentChange[] {
  const keys = [...Object.keys(original), ...Object.keys(edited).filter((k) => !(k in original))]
  const out: ArgumentChange[] = []
  for (const key of keys) {
    const had = key in original && original[key] !== undefined
    const has = key in edited && edited[key] !== undefined
    if (had && has && JSON.stringify(original[key]) === JSON.stringify(edited[key])) continue
    if (!had && !has) continue
    if (had && has) {
      out.push({ key, kind: 'changed', lines: diffLines(asText(original[key]), asText(edited[key])) })
    } else {
      const kind = has ? 'add' : 'del'
      const text = asText(has ? edited[key] : original[key])
      out.push({ key, kind: has ? 'added' : 'removed', lines: text.split('\n').map((t) => ({ kind, text: t })) })
    }
  }
  return out
}
