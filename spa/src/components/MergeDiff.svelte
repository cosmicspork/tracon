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
  import { untrack } from 'svelte'
  import { Compartment, EditorState, RangeSetBuilder, type Extension } from '@codemirror/state'
  import { Decoration, EditorView, GutterMarker, WidgetType, gutter, type BlockInfo } from '@codemirror/view'
  import { MergeView, getChunks, getOriginalDoc, unifiedMergeView } from '@codemirror/merge'
  import type { LanguageSupport } from '@codemirror/language'

  import type { DiffMode, FileSides, Gap } from '../lib/diffview'
  import { overrideFor } from '../lib/mergeChanges'
  import { languageFor, syntax } from '../lib/syntax'

  let {
    sides,
    mode,
    wrap = false,
    gaps = [],
    tail = null,
    onexpand,
    unavailable = null,
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
  } = $props()

  let host = $state<HTMLDivElement>()
  /** The editor whose rows are the file after the change: what a position is read in. */
  let after: EditorView | undefined
  /** Every editor showing now, for what reconfigures them in place. */
  let editors: EditorView[] = []
  const wrapping = new Compartment()

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
    const marks = { gaps, tail, onexpand, unavailable }
    if (mode === 'split') {
      const merge = new MergeView({
        a: { doc: sides.a, extensions: [extensions(lang, wrapped), headerRows(sides.aLines), numbers('a', sides.aLines)] },
        b: {
          doc: sides.b,
          extensions: [extensions(lang, wrapped), headerRows(sides.bLines, marks), numbers('b', sides.bLines)],
        },
        parent: host,
        gutter: true,
        highlightChanges: true,
        diffConfig,
      })
      after = merge.b
      editors = [merge.a, merge.b]
      const unfollow = followSideways(merge.a, merge.b)
      return () => {
        unfollow()
        merge.destroy()
        after = undefined
        editors = []
      }
    }
    const view = new EditorView({
      doc: sides.b,
      extensions: [
        unifiedMergeView({ original: sides.a, mergeControls: false, highlightChanges: true, gutter: true, diffConfig }),
        extensions(lang, wrapped),
        headerRows(sides.bLines, marks),
        unifiedNumbers(sides),
      ],
      parent: host,
    })
    after = view
    editors = [view]
    return () => {
      view.destroy()
      after = undefined
      editors = []
    }
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

  // --- line numbers -----------------------------------------------------------

  class Num extends GutterMarker {
    text: string
    constructor(text: string) {
      super()
      this.text = text
    }
    eq(other: GutterMarker) {
      return other instanceof Num && other.text === this.text
    }
    toDOM() {
      return document.createTextNode(this.text)
    }
  }

  /** Several numbers stacked, one per row of a unified view's deleted lines. */
  class Nums extends GutterMarker {
    texts: string[]
    constructor(texts: string[]) {
      super()
      this.texts = texts
    }
    eq(other: GutterMarker) {
      return other instanceof Nums && other.texts.join() === this.texts.join()
    }
    toDOM() {
      const box = document.createElement('div')
      box.className = 'cm-deletedNumbers'
      for (const t of this.texts) box.appendChild(document.createElement('div')).textContent = t
      return box
    }
  }

  const widest = (lines: (number | null)[]) => new Num(String(Math.max(9, ...lines.map((n) => n ?? 0))))
  const rowOf = (view: EditorView, line: BlockInfo) => view.state.doc.lineAt(line.from).number - 1

  /** A column of the file's own line numbers, from what each row of the document is in the file. */
  function numberColumn(cls: string, lines: (number | null)[], widget?: GutterConfigWidget): Extension {
    return gutter({
      class: `cm-lineNo ${cls}`,
      lineMarker: (view, line) => {
        const n = lines[rowOf(view, line)]
        return n == null ? null : new Num(String(n))
      },
      initialSpacer: () => widest(lines),
      // An absent marker for widgets must be left out, not passed as undefined:
      // the merge view's spacers are widgets too.
      ...(widget ? { widgetMarker: widget } : {}),
    })
  }
  type GutterConfigWidget = (view: EditorView, widget: WidgetType, block: BlockInfo) => GutterMarker | null

  /** Side by side: each side numbers its own lines. */
  function numbers(side: 'a' | 'b', lines: (number | null)[]): Extension {
    return numberColumn(side === 'a' ? 'cm-lineNo-before' : 'cm-lineNo-after', lines)
  }

  /**
   * Unified: a column of the lines before the change and one of the lines
   * after it. A context row has both; an added row only the after; the rows
   * of a deleted chunk, drawn as one widget, only the before.
   */
  function unifiedNumbers(s: FileSides): Extension {
    const deleted: GutterConfigWidget = (view, _widget, block) => {
      const chunk = getChunks(view.state)?.chunks.find((c) => c.fromB === block.from && c.fromA < c.toA)
      if (!chunk) return null
      const orig = getOriginalDoc(view.state)
      const first = orig.lineAt(chunk.fromA).number
      const last = orig.lineAt(Math.max(chunk.fromA, chunk.endA)).number
      const texts: string[] = []
      for (let n = first; n <= last; n++) texts.push(String(s.aLines[n - 1] ?? ''))
      return new Nums(texts)
    }
    return [numberColumn('cm-lineNo-before', s.bOld, deleted), numberColumn('cm-lineNo-after', s.bLines)]
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

  /** Where on screen a row of the after side starts, as `rowAt` named it. */
  export function rowTop(line: number): number | null {
    if (!after) return null
    const doc = after.state.doc
    const block = after.lineBlockAt(doc.line(Math.min(Math.max(line, 1), doc.lines)).from)
    return after.documentTop + block.top
  }

  // The editor takes the interface's tokens rather than bringing a theme. The
  // before side washes red and the after side green; unified, the deleted
  // chunks are red and the rest of the changed lines green.
  const theme = EditorView.theme({
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
  })
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
