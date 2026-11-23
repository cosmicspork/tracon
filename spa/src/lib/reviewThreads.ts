// The operator's review threads: general, on a file, or on a line or range,
// each bound to the revision it was written on and, unless general, to a
// path and a side of that revision's diff. The node anchors and judges
// them; this reads what it says and finds where a diff line would anchor
// one. A thread is never moved: on a later revision whose lines changed it
// reads `outdated`, and still names the lines it was written on.
//
// The seam for a diff component: give it `numberLines` (a plain diff) or
// `sideRows` (the merge view's two sides) to know each line's numbers, and
// have a click call back with the `Anchor` `anchorAt` makes. `threadsOn`
// says which threads a line carries, to mark it.

import type { DiffLine } from './diff'
import type { FileSides } from './diffview'

export type ThreadKind = 'general' | 'file' | 'line'
export type ThreadSide = 'old' | 'new'
/** As read on the latest revision; precedence resolved > addressed > outdated > unresolved. */
export type ThreadState = 'unresolved' | 'addressed' | 'outdated' | 'resolved'

export interface ThreadComment {
  author: 'operator' | 'agent'
  body: string
  /** The revision number the review was on when it was written. */
  revision: number | null
  at_ms: number
}

export interface ReviewThread {
  id: string
  kind: ThreadKind
  revision_id: string
  /** Its revision's number, from 1. */
  revision: number | null
  state: ThreadState
  /** Its lines no longer read as they did, on the latest revision. */
  outdated: boolean
  path?: string | null
  side?: ThreadSide | null
  start_line?: number | null
  end_line?: number | null
  /** A line thread's lines, as its revision showed them. */
  lines?: string | null
  addressed_revision?: number | null
  comments: ThreadComment[]
  created_ms: number
  updated_ms: number
}

/** Where a new thread goes, as the composer holds it. */
export interface Anchor {
  kind: ThreadKind
  path?: string
  side?: ThreadSide
  start_line?: number
  end_line?: number
}

/** A diff line's numbers on each side, where it has one. */
export interface Numbered extends DiffLine {
  /** The file it is in, from its `diff --git` header. */
  path: string | null
  old: number | null
  new: number | null
}

/**
 * Number each classified line of a unified diff on the side or sides it is
 * on: context on both, a removal on the old, an addition on the new. Header
 * and hunk lines have none. The path is read as `splitDiff` reads it.
 */
export function numberLines(lines: DiffLine[]): Numbered[] {
  let path: string | null = null
  let old = 0
  let now = 0
  return lines.map((line) => {
    const { text, kind } = line
    if (text.startsWith('diff --git ')) {
      path = text.split(' b/').pop() ?? null
    } else if (kind === 'meta' && text.startsWith('+++ b/')) {
      path = text.slice(6)
    } else if (kind === 'hunk') {
      const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(text)
      if (m) {
        old = Number(m[1])
        now = Number(m[2])
      }
    }
    const at = { ...line, path, old: null as number | null, new: null as number | null }
    if (kind === 'add') at.new = now++
    else if (kind === 'del') at.old = old++
    else if (kind === 'ctx') {
      at.old = old++
      at.new = now++
    }
    return at
  })
}

/**
 * Each row of a file's two merge-view sides as the `Numbered` line it is in
 * the diff, numbered as `numberLines` numbers it, or null on a row that is
 * no line (a hunk header, the empty row both sides end with). A row in a
 * block is a removal (`a`) or an addition (`b`); every other row is shared
 * by both sides in the same order, so the k-th such row of `a` is the k-th of
 * `b`, and a context row carries both numbers on either side.
 */
export function sideRows(sides: FileSides): { a: (Numbered | null)[]; b: (Numbered | null)[] } {
  const rowsA = sides.a.split('\n')
  const rowsB = sides.b.split('\n')
  const inA = new Set<number>()
  const inB = new Set<number>()
  for (const k of sides.blocks) {
    for (let i = k.a; i < k.a + k.aN; i++) inA.add(i)
    for (let i = k.b; i < k.b + k.bN; i++) inB.add(i)
  }
  const sharedA = sides.aLines.map((_, i) => i).filter((i) => !inA.has(i))
  const sharedB = sides.bLines.map((_, i) => i).filter((i) => !inB.has(i))
  const a: (Numbered | null)[] = sides.aLines.map(() => null)
  const b: (Numbered | null)[] = sides.bLines.map(() => null)
  const path = sides.path
  for (const i of inA) {
    const n = sides.aLines[i]
    if (n !== null) a[i] = { text: `-${rowsA[i]}`, kind: 'del', path, old: n, new: null }
  }
  for (const i of inB) {
    const n = sides.bLines[i]
    if (n !== null) b[i] = { text: `+${rowsB[i]}`, kind: 'add', path, old: null, new: n }
  }
  sharedA.forEach((i, k) => {
    const j = sharedB[k]
    const old = sides.aLines[i]
    const now = j === undefined ? null : sides.bLines[j]
    if (old === null || now === null) return
    const line: Numbered = { text: ` ${rowsA[i]}`, kind: 'ctx', path, old, new: now }
    a[i] = line
    b[j] = line
  })
  return { a, b }
}

/**
 * Whether a line is one of those a file's diff shows, on the side a click on
 * it anchors to (`anchorAt`): the node anchors a thread only to those. The
 * rows of a file whose gaps were opened (`expand`) number the lines the
 * diff leaves out too; given the sides of the diff as it came, this says
 * which of them a thread can start on.
 */
export function inDiff(sides: FileSides): (line: Numbered) => boolean {
  const { a, b } = sideRows(sides)
  const old = new Set<number>()
  const now = new Set<number>()
  for (const l of [...a, ...b]) {
    if (l?.old != null) old.add(l.old)
    if (l?.new != null) now.add(l.new)
  }
  return (line) => (line.new !== null ? now.has(line.new) : line.old !== null && old.has(line.old))
}

/**
 * The anchor a click on `line` starts: on the new side unless the line is
 * only on the old. Extending one already on the same path and side makes it
 * a range. Null for a line with no number.
 */
export function anchorAt(line: Numbered, extend?: Anchor | null): Anchor | null {
  if (!line.path) return null
  const side: ThreadSide = line.new !== null ? 'new' : 'old'
  const n = side === 'new' ? line.new : line.old
  if (n === null) return null
  if (extend?.kind === 'line' && extend.path === line.path && extend.side === side && extend.start_line) {
    const from = Math.min(extend.start_line, n)
    const to = Math.max(extend.end_line ?? extend.start_line, extend.start_line, n)
    return { kind: 'line', path: line.path, side, start_line: from, end_line: to }
  }
  return { kind: 'line', path: line.path, side, start_line: n, end_line: n }
}

/** The threads a numbered line carries: line threads covering it, written on the revision shown. */
export function threadsOn(threads: ReviewThread[], line: Numbered, revisionId: string | null | undefined): ReviewThread[] {
  return threads.filter((t) => {
    if (t.kind !== 'line' || t.path !== line.path || t.revision_id !== revisionId) return false
    const n = t.side === 'old' ? line.old : line.new
    return n !== null && n >= (t.start_line ?? 0) && n <= (t.end_line ?? t.start_line ?? 0)
  })
}

/** Where a thread is, in words: `a.txt:2–3 · new`, `a.txt`, or `General`. */
export function whereLabel(t: Pick<ReviewThread, 'kind' | 'path' | 'side' | 'start_line' | 'end_line'>): string {
  if (t.kind === 'general' || !t.path) return 'General'
  const side = t.side === 'old' ? ' · base' : ''
  if (t.kind === 'file') return `${t.path}${side}`
  const start = t.start_line ?? 0
  const end = t.end_line ?? start
  return `${t.path}:${start}${end !== start ? `–${end}` : ''}${side}`
}

export const STATE_LABEL: Record<ThreadState, string> = {
  unresolved: 'unresolved',
  addressed: 'addressed by the agent',
  outdated: 'outdated',
  resolved: 'resolved',
}

/** Threads the agent has still to answer. */
export function unresolvedCount(threads: ReviewThread[]): number {
  return threads.filter((t) => t.state === 'unresolved' || t.state === 'outdated').length
}

/** What a composer's anchor is short of, or null when it can be sent with `body`. */
export function anchorProblem(a: Anchor, body: string): string | null {
  if (!body.trim()) return 'say something'
  if (a.kind === 'general') return null
  if (!a.path) return 'choose a file'
  if (a.kind === 'line') {
    if (!a.start_line || a.start_line < 1) return 'choose a line'
    if (a.end_line !== undefined && a.end_line < a.start_line) return 'the range ends before it starts'
  }
  return null
}

/** The request body for an anchor: only what its kind names. */
export function createBody(a: Anchor, body: string, revisionId: string | null | undefined) {
  return {
    kind: a.kind,
    body: body.trim(),
    revision_id: revisionId ?? undefined,
    path: a.kind === 'general' ? undefined : a.path,
    side: a.kind === 'general' ? undefined : (a.side ?? 'new'),
    start_line: a.kind === 'line' ? a.start_line : undefined,
    end_line: a.kind === 'line' ? (a.end_line ?? a.start_line) : undefined,
  }
}
