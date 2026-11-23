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
  if (line.startsWith('Binary files ')) return 'binary file changed'
  return null
}

/** Build the two sides of one file from its slice of the unified diff. */
export function fileSides(file: DiffFile): FileSides {
  const a: string[] = []
  const b: string[] = []
  const aLines: (number | null)[] = []
  const bLines: (number | null)[] = []
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
    } else if (text.startsWith('\\')) {
      // "\ No newline at end of file": it belongs to the line before it, and
      // the line itself already reads as changed when it is the difference.
    } else {
      flush()
      a.push(text.slice(1))
      b.push(text.slice(1))
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
  }

  return { path: file.path, a: a.join('\n'), b: b.join('\n'), aLines, bLines, blocks, notes, hunks: aLines.length > 0 }
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
