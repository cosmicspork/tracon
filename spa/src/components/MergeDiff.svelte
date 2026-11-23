<script lang="ts">
  // One file of a review's diff on CodeMirror's merge view: side by side, the
  // two sides aligned row for row, or unified, deletions above the lines that
  // replace them. Read-only; editing a file is DiffEditor's.
  //
  // The page around it scrolls, never the editors, so side by side the two
  // sides scroll vertically as one. Sideways each side scrolls on its own, and
  // each follows the other.
  //
  // Further layers (a line-number gutter from `sides.aLines`/`bLines`, syntax
  // highlighting) are extensions added to `extensions()` below; both modes
  // take them.
  import { EditorState, RangeSetBuilder, type Extension } from '@codemirror/state'
  import { Decoration, EditorView } from '@codemirror/view'
  import { MergeView, unifiedMergeView } from '@codemirror/merge'

  import type { DiffMode, FileSides } from '../lib/diffview'
  import { overrideFor } from '../lib/mergeChanges'

  let { sides, mode }: { sides: FileSides; mode: DiffMode } = $props()

  let host = $state<HTMLDivElement>()
  /** The editor whose rows are the file after the change: what a position is read in. */
  let after: EditorView | undefined

  $effect(() => {
    if (!host) return
    const diffConfig = { override: overrideFor(sides) }
    if (mode === 'split') {
      const merge = new MergeView({
        a: { doc: sides.a, extensions: extensions(sides.aLines) },
        b: { doc: sides.b, extensions: extensions(sides.bLines) },
        parent: host,
        gutter: true,
        highlightChanges: true,
        diffConfig,
      })
      after = merge.b
      const unfollow = followSideways(merge.a, merge.b)
      return () => {
        unfollow()
        merge.destroy()
        after = undefined
      }
    }
    const view = new EditorView({
      doc: sides.b,
      extensions: [
        unifiedMergeView({ original: sides.a, mergeControls: false, highlightChanges: true, gutter: true, diffConfig }),
        extensions(sides.bLines),
      ],
      parent: host,
    })
    after = view
    return () => {
      view.destroy()
      after = undefined
    }
  })

  /** What every editor of the diff takes, whichever side and mode it is. */
  function extensions(lines: (number | null)[]): Extension {
    return [EditorState.readOnly.of(true), EditorView.editable.of(false), headerRows(lines), theme]
  }

  /** Hunk headers read as headers, not as code: they are the rows with no line in the file. */
  function headerRows(lines: (number | null)[]): Extension {
    return EditorView.decorations.compute([], (state) => {
      const set = new RangeSetBuilder<Decoration>()
      const last = state.doc.lines
      lines.forEach((n, i) => {
        if (n === null && i + 1 < last) set.add(state.doc.line(i + 1).from, state.doc.line(i + 1).from, header)
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
    '.cm-gutters': { backgroundColor: 'var(--s2)', border: 'none' },
    '&.cm-focused': { outline: 'none' },
    '.cm-hunkHeader': { color: 'var(--dim)', backgroundColor: 'var(--s1)' },
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
