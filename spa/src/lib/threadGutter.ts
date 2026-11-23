// The merge view's review-thread gutter: a button per row that is a line of
// the file, reading its number on that side and marked when threads are on
// it. Kept apart from `reviewThreads.ts` because it needs CodeMirror, which
// the page fetches only when a diff shows.

import type { Extension } from '@codemirror/state'
import { gutter, GutterMarker } from '@codemirror/view'
import { getChunks, getOriginalDoc } from '@codemirror/merge'

import type { Numbered } from './reviewThreads'

type Pick = (line: Numbered) => void
type Marks = (line: Numbered) => number

/** One row's thread button: its line number, marked when threads are on it. */
class PickMarker extends GutterMarker {
  line: Numbered
  n: number
  count: number
  pick?: Pick

  constructor(line: Numbered, n: number, count: number, pick?: Pick) {
    super()
    this.line = line
    this.n = n
    this.count = count
    this.pick = pick
  }

  eq(other: GutterMarker): boolean {
    return (
      other instanceof PickMarker &&
      other.n === this.n &&
      other.count === this.count &&
      other.pick === this.pick &&
      other.line.path === this.line.path &&
      other.line.old === this.line.old &&
      other.line.new === this.line.new
    )
  }

  toDOM(): Node {
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

/** The removed rows of a unified chunk, one button above another, each a row high. */
class ColumnMarker extends GutterMarker {
  markers: (PickMarker | null)[]
  height: number

  constructor(markers: (PickMarker | null)[], height: number) {
    super()
    this.markers = markers
    this.height = height
  }

  eq(other: GutterMarker): boolean {
    return (
      other instanceof ColumnMarker &&
      other.height === this.height &&
      other.markers.length === this.markers.length &&
      other.markers.every((m, i) => {
        const mine = this.markers[i]
        return m === null || mine === null ? m === mine : m.eq(mine)
      })
    )
  }

  toDOM(): Node {
    const col = document.createElement('div')
    col.className = 'cm-threadColumn'
    for (const m of this.markers) {
      const row = col.appendChild(document.createElement('div'))
      row.style.height = `${this.height}px`
      if (m) row.appendChild(m.toDOM())
    }
    return col
  }
}

const marker = (line: Numbered | null, n: number | null, pick?: Pick, marks?: Marks) =>
  line && n !== null ? new PickMarker(line, n, marks?.(line) ?? 0, pick) : null

/**
 * The gutter that starts and marks review threads on one editor of a file's
 * merge view. `side` is its document's rows, as `sideRows` numbers them.
 * Unified, `deleted` is the before side: its removed rows show in a widget
 * above the rows that replace them, and that widget's gutter gets a button
 * per removed row. Neither `pick` nor `marks` given, there is no gutter.
 */
export function threadGutter(
  side: (Numbered | null)[],
  deleted: (Numbered | null)[] | undefined,
  pick: Pick | undefined,
  marks: Marks | undefined,
): Extension {
  if (!pick && !marks) return []
  return gutter({
    class: 'cm-threadGutter',
    lineMarker(view, block) {
      const line = side[view.state.doc.lineAt(block.from).number - 1] ?? null
      return marker(line, line ? (line.new ?? line.old) : null, pick, marks)
    },
    widgetMarker(view, _widget, block) {
      if (!deleted) return null
      const chunk = getChunks(view.state)?.chunks.find((c) => c.fromB === block.from && c.fromA < c.toA)
      if (!chunk) return null
      const orig = getOriginalDoc(view.state)
      const first = orig.lineAt(chunk.fromA).number
      const last = orig.lineAt(chunk.endA).number
      const markers: (PickMarker | null)[] = []
      for (let n = first; n <= last; n++) {
        const line = deleted[n - 1] ?? null
        markers.push(marker(line, line ? (line.old ?? line.new) : null, pick, marks))
      }
      return new ColumnMarker(markers, view.defaultLineHeight)
    },
    // Room for a five-digit line number from the start, so the gutter does not grow as rows show.
    initialSpacer: () => new PickMarker({ text: '', kind: 'ctx', path: null, old: 99999, new: 99999 }, 99999, 0),
  })
}
