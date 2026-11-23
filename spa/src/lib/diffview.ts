// The review diff, as two documents CodeMirror's merge view can lay side by
// side or fold into one. A unified diff carries only its hunks, so each side
// is the hunks' lines on that side, with every hunk header kept as a row both
// sides share: the sides stay aligned at each hunk, and the header says where
// in the file the hunk sits.
//
// Nothing here imports CodeMirror: the page reads a file's header from this
// before the merge view has been fetched. `mergeChanges.ts` turns the rows
// into what the merge view marks.

import type { DiffFile } from './diff'

/** A run of removed rows and the added rows that replace them, by where each starts on its side. */
export interface Block {
  a: number
  aN: number
  b: number
  bN: number
}

/** One file of a review's diff, as the two documents the merge view compares. */
export interface FileSides {
  path: string
  /** The hunks' lines before the change, hunk headers included. */
  a: string
  /** The hunks' lines after the change, hunk headers included. */
  b: string
  /**
   * For each row of `a` and of `b`, the line it is in the real file, 1-based,
   * or null on a hunk header and on the empty row both sides end with. What
   * a line-number gutter shows.
   */
  aLines: (number | null)[]
  bLines: (number | null)[]
  /**
   * For each row of `b`, the line it was before the change when it is a
   * context row (the same on both sides), else null: the "before" column of
   * a unified view's two columns of line numbers.
   */
  bOld: (number | null)[]
  /** Rows the diff removed and added: every row in no block is context or a header. */
  blocks: Block[]
  /** What the file's header says that no hunk shows: new, deleted, renamed, binary. */
  notes: string[]
  /** Whether the diff has any hunk for this file; a binary or pure rename has none. */
  hunks: boolean
}

const HUNK = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/

/** The header lines worth saying when no hunk shows what happened. */
function note(line: string): string | null {
  if (line.startsWith('new file mode')) return 'new file'
  if (line.startsWith('deleted file mode')) return 'deleted'
  if (line.startsWith('rename from ')) return `renamed from ${line.slice(12)}`
  if (line.startsWith('copy from ')) return `copied from ${line.slice(10)}`
  if (line.startsWith('new mode ')) return `mode now ${line.slice(9)}`
  if (line.startsWith('Binary files ') || line === 'GIT binary patch') return 'binary'
  return null
}

/**
 * What the body of a file with no hunks says instead of an empty editor: a
 * binary file, a rename or a change of mode, each said for what it is.
 */
export function noLines(sides: Pick<FileSides, 'notes'>): string {
  if (sides.notes.includes('binary')) return 'Binary file: its contents are not shown as text.'
  const renamed = sides.notes.find((n) => n.startsWith('renamed from ') || n.startsWith('copied from '))
  if (renamed) return `${renamed[0].toUpperCase()}${renamed.slice(1)}; the contents did not change.`
  if (sides.notes.some((n) => n.startsWith('mode now '))) return 'Only the file mode changed.'
  if (sides.notes.includes('new file')) return 'A new, empty file.'
  if (sides.notes.includes('deleted')) return 'An empty file, deleted.'
  return 'No lines to show.'
}

/** Build the two sides of one file from its slice of the unified diff. */
export function fileSides(file: DiffFile): FileSides {
  const a: string[] = []
  const b: string[] = []
  const aLines: (number | null)[] = []
  const bLines: (number | null)[] = []
  const bOld: (number | null)[] = []
  const notes: string[] = []
  const blocks: Block[] = []
  let block: Block | null = null
  let inHunk = false
  let lineA = 0
  let lineB = 0

  // The file's slice ends with the empty string the final newline leaves;
  // a blank context line is " ", so a bare "" past the end is not one.
  const lines = [...file.lines]
  while (lines.length > 0 && lines[lines.length - 1] === '') lines.pop()

  const flush = () => {
    if (block) blocks.push(block)
    block = null
  }
  const open = () => (block ??= { a: a.length, aN: 0, b: b.length, bN: 0 })

  for (const text of lines) {
    const hunk = HUNK.exec(text)
    if (hunk) {
      flush()
      inHunk = true
      lineA = Number(hunk[1])
      lineB = Number(hunk[2])
      // A new file's hunk starts at -0 and a deletion's at +0; the first real
      // line on either side is still line 1.
      lineA = Math.max(lineA, 1)
      lineB = Math.max(lineB, 1)
      a.push(text)
      b.push(text)
      aLines.push(null)
      bLines.push(null)
      bOld.push(null)
      continue
    }
    if (!inHunk) {
      const n = note(text)
      if (n) notes.push(n)
      continue
    }
    if (text.startsWith('-')) {
      open().aN++
      a.push(text.slice(1))
      aLines.push(lineA++)
    } else if (text.startsWith('+')) {
      open().bN++
      b.push(text.slice(1))
      bLines.push(lineB++)
      bOld.push(null)
    } else if (text.startsWith('\\')) {
      // "\ No newline at end of file": it belongs to the line before it, and
      // the line itself already reads as changed when it is the difference.
    } else {
      flush()
      a.push(text.slice(1))
      b.push(text.slice(1))
      bOld.push(lineA)
      aLines.push(lineA++)
      bLines.push(lineB++)
    }
  }
  flush()
  // Both sides end on a shared empty row, so an addition or removal at the
  // end of the last hunk is still followed by a row the sides agree on, and
  // the merge view does not take the row before it as changed.
  if (a.length > 0) {
    a.push('')
    b.push('')
    aLines.push(null)
    bLines.push(null)
    bOld.push(null)
  }

  return {
    path: file.path,
    a: a.join('\n'),
    b: b.join('\n'),
    aLines,
    bLines,
    bOld,
    blocks,
    notes,
    hunks: aLines.length > 0,
  }
}

// --- which presentation ----------------------------------------------------

/** Side by side, or one column with deletions above the lines that replace them. */
export type DiffMode = 'split' | 'unified'

/**
 * How wide the diff must be for side by side to be the default: two columns
 * of a monospace line each, with room for their gutters, and no narrower.
 */
export const SPLIT_MIN_WIDTH = 760

/** The mode a viewer who has not chosen one sees, by the diff's width. */
export function defaultMode(width: number): DiffMode {
  return width >= SPLIT_MIN_WIDTH ? 'split' : 'unified'
}

export const MODE_KEY = 'tracon-diff-mode'

/** The mode this viewer chose, if they did and storage can say. */
export function storedMode(storage?: Pick<Storage, 'getItem'>): DiffMode | null {
  try {
    // Reading `localStorage` itself throws where site data is blocked.
    const v = (storage ?? globalThis.localStorage)?.getItem(MODE_KEY)
    return v === 'split' || v === 'unified' ? v : null
  } catch {
    return null
  }
}

/** Remember this viewer's mode; blocked storage only means it is not remembered. */
export function storeMode(mode: DiffMode, storage?: Pick<Storage, 'setItem'>) {
  try {
    ;(storage ?? globalThis.localStorage)?.setItem(MODE_KEY, mode)
  } catch {
    /* private window or blocked storage: the choice lasts this page */
  }
}

export const WRAP_KEY = 'tracon-diff-wrap'

/** Whether this viewer wraps long lines; off until they turn it on. */
export function storedWrap(storage?: Pick<Storage, 'getItem'>): boolean {
  try {
    return (storage ?? globalThis.localStorage)?.getItem(WRAP_KEY) === 'on'
  } catch {
    return false
  }
}

/** Remember whether this viewer wraps long lines. */
export function storeWrap(on: boolean, storage?: Pick<Storage, 'setItem'>) {
  try {
    ;(storage ?? globalThis.localStorage)?.setItem(WRAP_KEY, on ? 'on' : 'off')
  } catch {
    /* blocked storage: the choice lasts this page */
  }
}

// --- context around the hunks ----------------------------------------------
//
// A diff shows a few lines either side of each change. The rest of the file
// is read, when the reader asks, from what the revision pinned (the node's
// `/context`, never the worktree as it stands), and a gap the reader opens is
// written back into the file's diff as context lines, so everything above
// (the two sides, the line numbers, the marked changes) reads it as if git
// had shown it in the first place.

/** One hunk of a file's diff, as its header says and with its lines. */
export interface Hunk {
  oldStart: number
  oldCount: number
  newStart: number
  newCount: number
  /** What follows the header's second `@@`: git's guess at the enclosing function. */
  section: string
  lines: string[]
}

const HUNK_FULL = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/

/** A file's diff as its header lines and its hunks. */
export function parseHunks(file: DiffFile): { head: string[]; hunks: Hunk[] } {
  const head: string[] = []
  const hunks: Hunk[] = []
  const lines = [...file.lines]
  while (lines.length > 0 && lines[lines.length - 1] === '') lines.pop()
  for (const text of lines) {
    const m = HUNK_FULL.exec(text)
    if (m) {
      hunks.push({
        oldStart: Number(m[1]),
        oldCount: m[2] === undefined ? 1 : Number(m[2]),
        newStart: Number(m[3]),
        newCount: m[4] === undefined ? 1 : Number(m[4]),
        section: m[5],
        lines: [],
      })
    } else if (hunks.length > 0) hunks[hunks.length - 1].lines.push(text)
    else head.push(text)
  }
  return { head, hunks }
}

// A header's start is the line before the hunk when its count is 0 (an
// insertion after line 5 is "-5,0"); these are the first line the hunk takes
// up on each side either way, and its last.
const oldFirst = (h: Hunk) => (h.oldCount > 0 ? h.oldStart : h.oldStart + 1)
const newFirst = (h: Hunk) => (h.newCount > 0 ? h.newStart : h.newStart + 1)
const oldEnd = (h: Hunk) => oldFirst(h) + h.oldCount - 1
const newEnd = (h: Hunk) => newFirst(h) + h.newCount - 1

/** A run of unchanged lines the diff leaves out, before hunk `gap` or (at the hunk count) after the last. */
export interface Gap {
  gap: number
  /** How many lines; null after the last hunk until the file's length is known. */
  hidden: number | null
}

/** Both sides of a file whole, as the revision pinned them; a side the node could not read is null. */
export interface Context {
  base: string | null
  head: string | null
}

/** A file's text as its lines, without the empty one a final newline leaves. */
function linesOf(text: string): string[] {
  const lines = text.split('\n')
  if (lines.length > 0 && lines[lines.length - 1] === '') lines.pop()
  return lines
}

/**
 * Whether the pinned files are the ones the diff was taken between: every
 * line a hunk shows on a side must be that line of that side. Null when they
 * agree, else why not. Context shown from a file that does not match would
 * be a guess presented as evidence.
 */
export function contextMismatch(hunks: Hunk[], base: string[] | null, head: string[] | null): string | null {
  for (const h of hunks) {
    let a = oldFirst(h)
    let b = newFirst(h)
    for (const text of h.lines) {
      const mark = text[0]
      const body = text.slice(1)
      if (mark === '\\') continue
      if (mark !== '+' && base && base[a - 1] !== body) return `the base file differs from the diff at line ${a}`
      if (mark !== '-' && head && head[b - 1] !== body) return `the changed file differs from the diff at line ${b}`
      if (mark !== '+') a++
      if (mark !== '-') b++
    }
  }
  return null
}

/** The file's diff with the gaps in `open` filled from `context`, and the gaps still closed. */
export interface Expanded {
  file: DiffFile
  /** One per hunk header of `file`, in order: the gap that header stands for. */
  gaps: Gap[]
  /** The gap after the last hunk, when it is still closed. */
  tail: Gap | null
}

/**
 * Fill the gaps in `open` with the lines `context` has for them, merging the
 * hunks either side, and say which gaps are left and how big each is. With
 * no context, or none for this file, nothing is filled and the gap after the
 * last hunk is of unknown size.
 */
export function expand(file: DiffFile, context: Context | null, open: ReadonlySet<number>): Expanded {
  const { head: meta, hunks } = parseHunks(file)
  const base = context?.base != null ? linesOf(context.base) : null
  const after = context?.head != null ? linesOf(context.head) : null
  const known = base !== null || after !== null
  if (hunks.length === 0) return { file, gaps: [], tail: null }

  // The unchanged lines of the file, numbered by the side that has them.
  const source = after ?? base
  const lineAt = (n: number) => source![n - 1]
  const firstOf = after ? newFirst : oldFirst
  const endOf = after ? newEnd : oldEnd
  const length = source?.length ?? 0

  type Out = { oldFirst: number; newFirst: number; oldCount: number; newCount: number; section: string; lines: string[]; gap: Gap }
  const out: Out[] = []
  let cur: Out | null = null
  const startAt = (o: number, n: number, section: string, gap: Gap): Out => {
    const next: Out = { oldFirst: o, newFirst: n, oldCount: 0, newCount: 0, section, lines: [], gap }
    out.push(next)
    return next
  }
  /** Append unchanged lines from `from` to `to` of `source`, at old line `o` and new line `n`. */
  const fill = (from: number, to: number, o: number, n: number, gap: Gap) => {
    if (to < from) return
    // A hunk that now starts with the lines of an opened gap leaves none out above it.
    cur ??= startAt(o, n, '', { gap: gap.gap, hidden: 0 })
    for (let i = from; i <= to; i++) cur.lines.push(` ${lineAt(i)}`)
    cur.oldCount += to - from + 1
    cur.newCount += to - from + 1
  }

  hunks.forEach((h, k) => {
    const prev = hunks[k - 1]
    const hidden = firstOf(h) - (prev ? endOf(prev) + 1 : 1)
    const gap: Gap = { gap: k, hidden }
    const oStart = prev ? oldEnd(prev) + 1 : 1
    const nStart = prev ? newEnd(prev) + 1 : 1
    if (known && open.has(k) && hidden > 0) {
      const from = prev ? endOf(prev) + 1 : 1
      fill(from, from + hidden - 1, oStart, nStart, gap)
    } else if (hidden > 0 || !cur) {
      cur = null
    }
    if (!cur) cur = startAt(oldFirst(h), newFirst(h), h.section, gap)
    else if (!cur.section) cur.section = h.section
    for (const text of h.lines) {
      cur.lines.push(text)
      if (text.startsWith('\\')) continue
      if (!text.startsWith('+')) cur.oldCount++
      if (!text.startsWith('-')) cur.newCount++
    }
  })

  // A file the change adds or deletes is in the diff whole: nothing follows.
  const whole = meta.some((l) => l.startsWith('new file mode') || l.startsWith('deleted file mode'))
  const last = hunks[hunks.length - 1]
  const tailHidden = whole ? 0 : known ? Math.max(0, length - endOf(last)) : null
  let tail: Gap | null = { gap: hunks.length, hidden: tailHidden }
  if (tailHidden !== null && tailHidden > 0 && open.has(hunks.length)) {
    fill(endOf(last) + 1, length, oldEnd(last) + 1, newEnd(last) + 1, tail)
    tail = null
  } else if (tailHidden === 0) tail = null

  const header = (o: Out) => {
    const os = o.oldCount > 0 ? o.oldFirst : o.oldFirst - 1
    const ns = o.newCount > 0 ? o.newFirst : o.newFirst - 1
    return `@@ -${os},${o.oldCount} +${ns},${o.newCount} @@${o.section}`
  }
  const lines = [...meta, ...out.flatMap((o) => [header(o), ...o.lines]), '']
  return { file: { ...file, lines }, gaps: out.map((o) => o.gap), tail }
}
