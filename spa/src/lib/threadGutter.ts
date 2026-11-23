// The merge view's line numbers, which are also where review threads start:
// each side's own numbers (unified, a before column and an after column),
// and given a way to pick or mark a line, each number of a line the diff
// shows is a button that starts a thread on that line and reads marked when
// threads are on it. Kept apart from `reviewThreads.ts` because it needs
// CodeMirror, which the page fetches only when a diff shows.

import type { Extension } from '@codemirror/state'
import { gutter, GutterMarker, type BlockInfo, type EditorView, type WidgetType } from '@codemirror/view'
import { getChunks, getOriginalDoc } from '@codemirror/merge'

import type { FileSides } from './diffview'
import type { Numbered, sideRows } from './reviewThreads'

type Pick = (line: Numbered) => void
type Marks = (line: Numbered) => number

/** What a number does beyond being read. */
export interface Threads {
  /** Start a thread on the line; absent, the numbers start none. */
  pick?: Pick
  /** How many threads a line carries, to mark it. */
  marks?: Marks
  /** Which lines a thread can start on; absent, every line the rows number. */
  pickable?: (line: Numbered) => boolean
}

/** Each row of both sides as the line it is, from `sideRows`; null when no number picks or marks. */
export type Rows = ReturnType<typeof sideRows> | null

/** One row's line number: plain, or the button that starts and marks threads on its line. */
class Num extends GutterMarker {
  n: number
  line: Numbered | null
  count: number
  pick?: Pick

  constructor(n: number, line: Numbered | null = null, count = 0, pick?: Pick) {
    super()
    this.n = n
    this.line = line
    this.count = count
    this.pick = pick
  }

  eq(other: GutterMarker): boolean {
    if (!(other instanceof Num) || other.n !== this.n || other.count !== this.count || other.pick !== this.pick) return false
    const [a, b] = [this.line, other.line]
    return a === null || b === null ? a === b : a.path === b.path && a.old === b.old && a.new === b.new
  }

  toDOM(): Node {
    if (!this.line) return document.createTextNode(String(this.n))
    const b = document.createElement('button')
    b.type = 'button'
    b.className = 'cm-threadPick'
    if (this.count > 0) b.classList.add('cm-threadMarked')
    b.textContent = String(this.n)
    b.dataset.old = this.line.old === null ? '' : String(this.line.old)
    b.dataset.new = this.line.new === null ? '' : String(this.line.new)
    b.title =
      this.count > 0
        ? `${this.count} thread${this.count === 1 ? '' : 's'} here${this.pick ? ' · add another' : ''}`
        : this.pick
          ? 'Comment on this line'
          : ''
    const { pick, line } = this
    if (pick) {
      // The editor would take the press as a selection; the button keeps it.
      b.addEventListener('mousedown', (e) => e.preventDefault())
      b.addEventListener('click', () => pick(line))
    } else {
      b.disabled = true
    }
    return b
  }
}

/** Several numbers stacked, one per row of a unified view's deleted lines. */
class Nums extends GutterMarker {
  nums: (Num | null)[]

  constructor(nums: (Num | null)[]) {
    super()
    this.nums = nums
  }

  eq(other: GutterMarker): boolean {
    return (
      other instanceof Nums &&
      other.nums.length === this.nums.length &&
      other.nums.every((m, i) => {
        const mine = this.nums[i]
        return m === null || mine === null ? m === mine : m.eq(mine)
      })
    )
  }

  toDOM(): Node {
    const box = document.createElement('div')
    box.className = 'cm-deletedNumbers'
    for (const m of this.nums) {
      const row = box.appendChild(document.createElement('div'))
      if (m) row.appendChild(m.toDOM())
    }
    return box
  }
}

/** The number `n` of a row that is `line`: a button when threads can be started or marked on it. */
function num(n: number | null | undefined, line: Numbered | null | undefined, t: Threads): Num | null {
  if (n == null) return null
  if (!line || (!t.pick && !t.marks) || (t.pickable && !t.pickable(line))) return new Num(n)
  return new Num(n, line, t.marks?.(line) ?? 0, t.pick)
}

const widest = (lines: (number | null)[]) => new Num(Math.max(9, ...lines.map((n) => n ?? 0)))
const rowOf = (view: EditorView, line: BlockInfo) => view.state.doc.lineAt(line.from).number - 1
type WidgetMarker = (view: EditorView, widget: WidgetType, block: BlockInfo) => GutterMarker | null

/**
 * A column of the file's own line numbers: `lines` is the number each row
 * of the document shows in this column, `rows` the line each row is.
 */
function numberColumn(
  cls: string,
  lines: (number | null)[],
  rows: (Numbered | null)[] | undefined,
  t: Threads,
  widget?: WidgetMarker,
): Extension {
  return gutter({
    class: `cm-lineNo ${cls}`,
    lineMarker: (view, line) => {
      const i = rowOf(view, line)
      return num(lines[i], rows?.[i], t)
    },
    initialSpacer: () => widest(lines),
    // An absent marker for widgets must be left out, not passed as undefined:
    // the merge view's spacers are widgets too.
    ...(widget ? { widgetMarker: widget } : {}),
  })
}

/** Side by side: each side numbers its own lines, and a context line is the same line on either. */
export function sideNumbers(side: 'a' | 'b', sides: FileSides, rows: Rows, t: Threads): Extension {
  return side === 'a'
    ? numberColumn('cm-lineNo-before', sides.aLines, rows?.a, t)
    : numberColumn('cm-lineNo-after', sides.bLines, rows?.b, t)
}

/**
 * Unified: a column of the lines before the change and one of the lines
 * after it. A context row has both; an added row only the after; the rows
 * of a deleted chunk, drawn as one widget, only the before, one number (and
 * one pick) each.
 */
export function unifiedNumbers(sides: FileSides, rows: Rows, t: Threads): Extension {
  const deleted: WidgetMarker = (view, _widget, block) => {
    const chunk = getChunks(view.state)?.chunks.find((c) => c.fromB === block.from && c.fromA < c.toA)
    if (!chunk) return null
    const orig = getOriginalDoc(view.state)
    const first = orig.lineAt(chunk.fromA).number
    const last = orig.lineAt(Math.max(chunk.fromA, chunk.endA)).number
    const nums: (Num | null)[] = []
    for (let n = first; n <= last; n++) nums.push(num(sides.aLines[n - 1], rows?.a[n - 1], t))
    return new Nums(nums)
  }
  return [
    numberColumn('cm-lineNo-before', sides.bOld, rows?.b, t, deleted),
    numberColumn('cm-lineNo-after', sides.bLines, rows?.b, t),
  ]
}
