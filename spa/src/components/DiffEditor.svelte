<script lang="ts">
  // Editing a reviewed file in place. The operator sees the agent's change as
  // a unified diff and edits the result directly; what leaves here is a patch,
  // not a written file. The agent applies it, so the agent remains the only
  // writer to the worktree.
  import { EditorState } from '@codemirror/state'
  import { EditorView, keymap, lineNumbers } from '@codemirror/view'
  import { unifiedMergeView } from '@codemirror/merge'
  import { onDestroy } from 'svelte'

  let {
    path,
    original,
    head,
    readonly = false,
    onchange,
  }: {
    path: string
    /** The file before the agent's change: what the diff is shown against. */
    original: string
    /** The file as submitted, and the starting point for edits. */
    head: string
    readonly?: boolean
    onchange: (path: string, text: string) => void
  } = $props()

  let host = $state<HTMLDivElement>()
  let view: EditorView | undefined

  $effect(() => {
    if (!host) return
    const state = EditorState.create({
      doc: head,
      extensions: [
        lineNumbers(),
        unifiedMergeView({ original, mergeControls: false, highlightChanges: true }),
        keymap.of([]),
        EditorView.editable.of(!readonly),
        EditorState.readOnly.of(readonly),
        EditorView.lineWrapping,
        EditorView.updateListener.of((u) => {
          if (u.docChanged) onchange(path, u.state.doc.toString())
        }),
        theme,
      ],
    })
    view = new EditorView({ state, parent: host })
    return () => {
      view?.destroy()
      view = undefined
    }
  })

  onDestroy(() => view?.destroy())

  // The editor takes the interface's tokens rather than bringing a theme.
  const theme = EditorView.theme({
    '&': { backgroundColor: 'var(--s1)', color: 'var(--ink)', font: '12.5px var(--mono)' },
    '.cm-content': { padding: '8px 0' },
    '.cm-gutters': {
      backgroundColor: 'var(--s1)',
      color: 'var(--dim)',
      border: 'none',
      paddingRight: '6px',
    },
    '.cm-activeLine': { backgroundColor: 'var(--s2)' },
    '.cm-activeLineGutter': { backgroundColor: 'var(--s2)' },
    '&.cm-focused': { outline: 'none' },
    '.cm-changedLine': { backgroundColor: 'var(--wash-ok)' },
    '.cm-deletedChunk': { backgroundColor: 'var(--wash-crit)', color: 'var(--ink2)' },
    '.cm-changedText': { backgroundColor: 'var(--wash-ok)' },
    '.cm-selectionBackground': { backgroundColor: 'var(--s3)' },
    '.cm-cursor': { borderLeftColor: 'var(--acc)' },
  })
</script>

<div class="editor" class:readonly bind:this={host}></div>

<style>
  .editor {
    border-radius: 4px;
    overflow: hidden;
    background: var(--s1);
  }
  .editor.readonly {
    opacity: 0.75;
  }
</style>
