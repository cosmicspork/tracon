<script lang="ts" module>
  import { EditorView as View } from '@codemirror/view'

  // The editor takes the interface's tokens rather than bringing a theme.
  // The before side washes red and the after side green; unified, the deleted
  // chunks are red and the rest of the changed lines green. Made once for
  // every file: each theme is a style sheet of its own, and one per file
  // would grow the page's styles with every file drawn.
  const theme = View.theme({
    '&': { backgroundColor: 'var(--s2)', color: 'var(--ink)', font: '12px/1.55 var(--mono)' },
    '.cm-content': { padding: '4px 0' },
    '.cm-line': { padding: '0 12px 0 6px' },
    '.cm-gutters': { backgroundColor: 'var(--s2)', border: 'none', color: 'var(--dim)' },
    '.cm-lineNo .cm-gutterElement': { padding: '0 4px 0 8px', textAlign: 'right', minWidth: '2ch', userSelect: 'none' },
    '.cm-lineNo-before + .cm-lineNo-after .cm-gutterElement': { paddingLeft: '4px' },
    '.cm-deletedNumbers div': { lineHeight: '1.55' },
    '&.cm-focused': { outline: 'none' },
    '.cm-hunkHeader': { color: 'var(--dim)', backgroundColor: 'var(--s1)' },
    '.cm-gap': { marginLeft: '12px', color: 'var(--dim)', fontStyle: 'italic' },
    '.cm-gapMissing': { marginLeft: '8px', color: 'var(--wait)' },
    '.cm-gapShow': {
      marginLeft: '8px',
      font: '11px var(--mono)',
      fontStyle: 'normal',
      padding: '0 6px',
      border: '1px solid var(--rule)',
      borderRadius: '3px',
      background: 'var(--s2)',
      color: 'var(--ink)',
      cursor: 'pointer',
    },
    '.cm-changedLine': { backgroundColor: 'color-mix(in srgb, var(--ok) 16%, transparent)' },
    '.cm-changedText': { backgroundColor: 'color-mix(in srgb, var(--ok) 34%, transparent)' },
    '&.cm-merge-a .cm-changedLine, .cm-deletedChunk': {
      backgroundColor: 'color-mix(in srgb, var(--crit) 16%, transparent)',
    },
    '&.cm-merge-a .cm-changedText, .cm-deletedChunk .cm-deletedText': {
      backgroundColor: 'color-mix(in srgb, var(--crit) 34%, transparent)',
    },
    '&.cm-merge-b .cm-changedText': { backgroundColor: 'color-mix(in srgb, var(--ok) 34%, transparent)' },
    '.cm-deletedChunk': { paddingLeft: '6px' },
    '.cm-changedLineGutter': { backgroundColor: 'var(--ok)' },
    '&.cm-merge-a .cm-changedLineGutter, .cm-deletedLineGutter': { backgroundColor: 'var(--crit)' },
    '.cm-insertedLine, .cm-deletedLine, .cm-deletedLine del': { textDecoration: 'none' },
    '.cm-selectionBackground, &.cm-focused .cm-selectionBackground': { backgroundColor: 'var(--s3)' },
    // A line number that starts a review thread reads as the number it is.
    '.cm-threadPick': {
      display: 'block',
      width: '100%',
      padding: '0',
      border: 'none',
      background: 'none',
      color: 'inherit',
      font: 'inherit',
      textAlign: 'right',
      cursor: 'pointer',
    },
    '.cm-threadPick:hover:not(:disabled)': { color: 'var(--ink)', textDecoration: 'underline' },
    '.cm-threadPick:disabled': { cursor: 'default' },
    '.cm-threadPick.cm-threadMarked': { color: 'var(--ink)', fontWeight: '600', textDecoration: 'underline' },
  })
</script>

<script lang="ts">
  // One file of a review's diff on CodeMirror's merge view: side by side, the
  // two sides aligned row for row, or unified, deletions above the lines that
  // replace them. Read-only; editing a file is DiffEditor's.
  //
  // The page around it scrolls, never the editors, so side by side the two
  // sides scroll vertically as one. Sideways each side scrolls on its own, and
  // each follows the other; with wrapping on, nothing scrolls sideways.
  //
  // Every editor carries the file's own line numbers (one column per side;
  // unified, a before column and an after column), the file's syntax, and on
  // each hunk header how many unchanged lines the diff leaves out there, with
  // a way to show them when the page can read them.
  //
  // Given `onpick` or `marks`, those line numbers are also the review
  // threads' (`lib/threadGutter.ts`): each number of a line the diff shows is
  // a button that starts a thread on that line, as the node numbers it, and
  // reads marked when threads are on it. Unified, each removed line shown
  // above its replacement has its own. A new `onpick` or `marks` re-marks the
  // numbers in place, without rebuilding the editors or moving the view.
  import { untrack } from 'svelte'
  import { Compartment, EditorState, RangeSetBuilder, type Extension } from '@codemirror/state'
  import { Decoration, EditorView, WidgetType } from '@codemirror/view'
  import { MergeView, getChunks, unifiedMergeView } from '@codemirror/merge'
  import type { LanguageSupport } from '@codemirror/language'

  import type { DiffMode, FileSides, Gap } from '../lib/diffview'
  import { overrideFor } from '../lib/mergeChanges'
  import { languageFor, syntax } from '../lib/syntax'
  import { sideRows, type Numbered } from '../lib/reviewThreads'
  import { sideNumbers, unifiedNumbers, type Rows, type Threads } from '../lib/threadGutter'

  let {
    sides,
    mode,
    wrap = false,
    gaps = [],
    tail = null,
    onexpand,
    unavailable = null,
    onpick,
    marks,
    pickable,
  }: {
    sides: FileSides
    mode: DiffMode
    /** Wrap long lines to the width rather than scroll sideways. */
    wrap?: boolean
    /** One per hunk header, in order: the unchanged lines the diff leaves out above it. */
    gaps?: Gap[]
    /** The unchanged lines after the last hunk, when they are left out. */
    tail?: Gap | null
    /** Show the lines of a gap; absent, the gaps are only counted. */
    onexpand?: (gap: number) => void
    /** Why the left-out lines cannot be shown, said on each gap instead of a button. */
    unavailable?: string | null
    /**
     * Given, each line number is a button that calls it with that line,
     * numbered as the node numbers it: how a review thread is started on a
     * line. Unified, the removed lines shown above their replacement get one
     * each too.
     */
    onpick?: (line: Numbered) => void
    /** How many threads a line carries, to mark it. A new function re-marks the rows. */
    marks?: (line: Numbered) => number
    /** Which lines a thread can start on (not those an opened gap shows); absent, every line. */
    pickable?: (line: Numbered) => boolean
  } = $props()

  let host = $state<HTMLDivElement>()
  /** The editor whose rows are the file after the change: what a position is read in. */
  let after: EditorView | undefined
  /** The changes the editors mark, in positions of `after`, for moving change to change. */
  let changes: () => readonly { fromB: number }[] = () => []
  /** Every editor showing now, for what reconfigures them in place. */
  let editors: EditorView[] = []
  const wrapping = new Compartment()
  /** Each editor's line numbers, remade in place when what they pick or mark changes. */
  const numbering = new Compartment()
  /** How each editor showing now makes its line numbers, for remaking them. */
  let numbered: { view: EditorView; make: (t: Threads) => Extension }[] = []
  /** What the numbers showing now were made to pick and mark. */
  let made: Threads = {}

  const rows = $derived(sideRows(sides))
  /** Each row as its line, read only when a number picks or marks one. */
  const rowsFor = (t: Threads): Rows => (t.pick || t.marks ? rows : null)

  // The file's language, fetched before the editors are made: unified, the
  // deleted lines are drawn once, with whatever language the editor had then.
  // Undefined while it is being looked for, null when there is none.
  let language = $state<LanguageSupport | null | undefined>(undefined)
  $effect(() => {
    const path = sides.path
    let live = true
    language = undefined
    languageFor(path).then((l) => {
      if (live) language = l
    })
    return () => {
      live = false
    }
  })

  $effect(() => {
    if (!host || language === undefined) return
    const lang = language
    const diffConfig = { override: overrideFor(sides) }
    const wrapped = untrack(() => wrap)
    const hunks = { gaps, tail, onexpand, unavailable }
    const s = sides
    const t = untrack(threads)
    made = t
    if (mode === 'split') {
      const makeA = (t: Threads) => sideNumbers('a', s, rowsFor(t), t)
      const makeB = (t: Threads) => sideNumbers('b', s, rowsFor(t), t)
      const merge = new MergeView({
        a: { doc: s.a, extensions: [extensions(lang, wrapped), headerRows(s.aLines), numbering.of(untrack(() => makeA(t)))] },
        b: {
          doc: s.b,
          extensions: [extensions(lang, wrapped), headerRows(s.bLines, hunks), numbering.of(untrack(() => makeB(t)))],
        },
        parent: host,
        gutter: true,
        highlightChanges: true,
        diffConfig,
      })
      after = merge.b
      editors = [merge.a, merge.b]
      numbered = [
        { view: merge.a, make: makeA },
        { view: merge.b, make: makeB },
      ]
      changes = () => merge.chunks
      const unfollow = followSideways(merge.a, merge.b)
      return () => {
        unfollow()
        merge.destroy()
        after = undefined
        editors = []
        numbered = []
      }
    }
    const make = (t: Threads) => unifiedNumbers(s, rowsFor(t), t)
    const view = new EditorView({
      doc: sides.b,
      extensions: [
        unifiedMergeView({ original: sides.a, mergeControls: false, highlightChanges: true, gutter: true, diffConfig }),
        extensions(lang, wrapped),
        headerRows(s.bLines, hunks),
        numbering.of(untrack(() => make(t))),
      ],
      parent: host,
    })
    after = view
    editors = [view]
    numbered = [{ view, make }]
    changes = () => getChunks(view.state)?.chunks ?? []
    return () => {
      view.destroy()
      after = undefined
      editors = []
      numbered = []
    }
  })

  /** What the line numbers are to pick and mark, as the props say now. */
  function threads(): Threads {
    return { pick: onpick, marks, pickable }
  }

  // A thread opened, a revision changed, threads blocked: the numbers are
  // remade in place, and the editors and the reader's row stay.
  $effect(() => {
    const t = threads()
    untrack(() => {
      if (t.pick === made.pick && t.marks === made.marks && t.pickable === made.pickable) return
      made = t
      for (const { view, make } of numbered) view.dispatch({ effects: numbering.reconfigure(make(t)) })
    })
  })

  // Wrapping changes in place: the editors stay, and so does the reader's row.
  $effect(() => {
    const on = wrap
    for (const e of untrack(() => editors)) e.dispatch({ effects: wrapping.reconfigure(on ? EditorView.lineWrapping : []) })
  })

  /** What every editor of the diff takes, whichever side and mode it is. */
  function extensions(lang: LanguageSupport | null, wrapped: boolean): Extension {
    return [
      EditorState.readOnly.of(true),
      EditorView.editable.of(false),
      wrapping.of(wrapped ? EditorView.lineWrapping : []),
      lang ? [lang, syntax] : [],
      theme,
    ]
  }

  // --- hunk headers and the lines they leave out ---------------------------

  /** What a hunk header says about the unchanged lines above it (or, at the end, below the last hunk). */
  class GapWidget extends WidgetType {
    gap: Gap
    below: boolean
    onexpand: ((gap: number) => void) | undefined
    unavailable: string | null
    constructor(gap: Gap, below: boolean, onexpand: ((gap: number) => void) | undefined, unavailable: string | null) {
      super()
      this.gap = gap
      this.below = below
      this.onexpand = onexpand
      this.unavailable = unavailable
    }
    eq(other: GapWidget) {
      return (
        other.gap.gap === this.gap.gap &&
        other.gap.hidden === this.gap.hidden &&
        other.unavailable === this.unavailable &&
        other.onexpand === this.onexpand
      )
    }
    toDOM() {
      const box = document.createElement('span')
      box.className = 'cm-gap'
      const { hidden } = this.gap
      const what =
        hidden === null
          ? 'the rest of the file'
          : `${hidden} unchanged line${hidden === 1 ? '' : 's'} ${this.below ? 'below' : 'above'}`
      box.appendChild(document.createElement('span')).textContent = `⋯ ${what}`
      if (this.unavailable) {
        const why = box.appendChild(document.createElement('span'))
        why.className = 'cm-gapMissing'
        why.textContent = `context unavailable: ${this.unavailable}`
      } else if (this.onexpand) {
        const show = box.appendChild(document.createElement('button'))
        show.type = 'button'
        show.className = 'cm-gapShow'
        show.textContent = 'Show'
        show.dataset.gap = String(this.gap.gap)
        const expand = this.onexpand
        const gap = this.gap.gap
        show.addEventListener('click', (e) => {
          e.preventDefault()
          expand(gap)
        })
      }
      return box
    }
    ignoreEvent() {
      return true
    }
  }

  /**
   * Hunk headers read as headers, not as code: they are the rows with no line
   * in the file. Given the gaps, the after side's headers also say what they
   * leave out, as does the empty row the sides end on for the lines after the
   * last hunk.
   */
  function headerRows(
    lines: (number | null)[],
    marks?: { gaps: Gap[]; tail: Gap | null; onexpand?: (gap: number) => void; unavailable: string | null },
  ): Extension {
    return EditorView.decorations.compute([], (state) => {
      const set = new RangeSetBuilder<Decoration>()
      const last = state.doc.lines
      let k = 0
      lines.forEach((n, i) => {
        if (n !== null || i + 1 > last) return
        const line = state.doc.line(i + 1)
        if (i + 1 < last) {
          set.add(line.from, line.from, header)
          const gap = marks?.gaps[k++]
          if (gap && gap.hidden !== 0) {
            set.add(line.to, line.to, Decoration.widget({ widget: new GapWidget(gap, false, marks!.onexpand, marks!.unavailable), side: 1 }))
          }
        } else if (marks?.tail) {
          set.add(line.from, line.from, header)
          set.add(line.to, line.to, Decoration.widget({ widget: new GapWidget(marks.tail, true, marks.onexpand, marks.unavailable), side: 1 }))
        }
      })
      return set.finish()
    })
  }
  const header = Decoration.line({ class: 'cm-hunkHeader' })

  /** Keep the two sides' sideways scroll together, whichever is scrolled. */
  function followSideways(a: EditorView, b: EditorView): () => void {
    let busy = false
    const follow = (from: HTMLElement, to: HTMLElement) => () => {
      if (busy || to.scrollLeft === from.scrollLeft) return
      busy = true
      to.scrollLeft = from.scrollLeft
      requestAnimationFrame(() => (busy = false))
    }
    const ab = follow(a.scrollDOM, b.scrollDOM)
    const ba = follow(b.scrollDOM, a.scrollDOM)
    a.scrollDOM.addEventListener('scroll', ab, { passive: true })
    b.scrollDOM.addEventListener('scroll', ba, { passive: true })
    return () => {
      a.scrollDOM.removeEventListener('scroll', ab)
      b.scrollDOM.removeEventListener('scroll', ba)
    }
  }

  /**
   * The row at a height on screen, as a line of the after side and how far
   * into it the height falls: what a switch of mode keeps in place.
   */
  export function rowAt(clientY: number): { line: number; into: number } | null {
    if (!after) return null
    const top = after.documentTop
    const block = after.lineBlockAtHeight(clientY - top)
    return { line: after.state.doc.lineAt(block.from).number, into: clientY - top - block.top }
  }

  /**
   * Where on screen each change starts, top to bottom: what moving to the
   * next or previous change steps through. Unified, a deletion is drawn above
   * the row it precedes, so it starts a deleted chunk's height higher; the
   * row is near enough to bring it into view.
   */
  export function changeTops(): number[] {
    if (!after) return []
    const view = after
    return changes().map((c) => view.documentTop + view.lineBlockAt(c.fromB).top)
  }

  /** Where on screen a row of the after side starts, as `rowAt` named it. */
  export function rowTop(line: number): number | null {
    if (!after) return null
    const doc = after.state.doc
    const block = after.lineBlockAt(doc.line(Math.min(Math.max(line, 1), doc.lines)).from)
    return after.documentTop + block.top
  }

</script>

<div class="merge {mode}" bind:this={host}></div>

<style>
  .merge {
    border-radius: 4px;
    overflow: hidden;
    background: var(--s2);
  }
  /* The page scrolls the diff, not the merge view: both sides move as one. */
  .merge :global(.cm-mergeView) {
    overflow-y: visible;
  }
  .split :global(.cm-mergeViewEditor + .cm-mergeViewEditor) {
    border-left: 1px solid var(--rule);
  }
</style>
