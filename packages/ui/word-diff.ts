// A word diff of a text before and after: the `word_diff` card format a
// plugin raises (`approval/request`), drawn by the page rather than taken
// from the plugin, so what is shown is exactly the change from one text to
// the other.

export type WordPart = { kind: 'same' | 'add' | 'del'; text: string }

/** Past this many token pairs the middle is shown as replaced whole. */
const MAX_CELLS = 4_000_000

/** Words and the runs of whitespace between them, in order. */
export function tokens(text: string): string[] {
  return text.match(/\s+|[^\s]+/g) ?? []
}

function push(out: WordPart[], kind: WordPart['kind'], text: string) {
  if (!text) return
  const last = out[out.length - 1]
  if (last && last.kind === kind) last.text += text
  else out.push({ kind, text })
}

/**
 * The word diff of `before` and `after`. Concatenating the `same` and `del`
 * parts gives `before`; the `same` and `add` parts give `after`.
 */
export function wordDiff(before: string, after: string): WordPart[] {
  const a = tokens(before)
  const b = tokens(after)
  let start = 0
  while (start < a.length && start < b.length && a[start] === b[start]) start++
  let endA = a.length
  let endB = b.length
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--
    endB--
  }
  const out: WordPart[] = []
  push(out, 'same', a.slice(0, start).join(''))
  const midA = a.slice(start, endA)
  const midB = b.slice(start, endB)
  if (midA.length * midB.length > MAX_CELLS) {
    push(out, 'del', midA.join(''))
    push(out, 'add', midB.join(''))
  } else {
    // Longest common subsequence, filled from the end so a walk from the
    // start reads it off.
    const n = midA.length
    const m = midB.length
    const lcs: Uint32Array[] = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1))
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        lcs[i][j] = midA[i] === midB[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1])
      }
    }
    let i = 0
    let j = 0
    while (i < n && j < m) {
      if (midA[i] === midB[j]) {
        push(out, 'same', midA[i])
        i++
        j++
      } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
        push(out, 'del', midA[i++])
      } else {
        push(out, 'add', midB[j++])
      }
    }
    while (i < n) push(out, 'del', midA[i++])
    while (j < m) push(out, 'add', midB[j++])
  }
  push(out, 'same', a.slice(endA).join(''))
  return out
}

/** How many words were taken out and put in, for the block's heading. */
export function wordCounts(parts: WordPart[]): { added: number; removed: number } {
  const count = (kind: WordPart['kind']) =>
    parts.filter((p) => p.kind === kind).reduce((n, p) => n + tokens(p.text).filter((t) => t.trim()).length, 0)
  return { added: count('add'), removed: count('del') }
}
