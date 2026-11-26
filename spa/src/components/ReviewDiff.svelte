<script lang="ts">
  // The Review page's diff: each file under its own header, side by side or
  // unified on CodeMirror's merge view.
  //
  // The mode is the viewer's: whatever they last chose, remembered on this
  // device, or until they choose, side by side when the diff is wide enough
  // and unified when it is not. Changing it keeps the files that are open and
  // the row at the top of the view where it was.
  //
  // CodeMirror is most of a megabyte, so it is fetched when a diff first
  // shows; until it arrives, and if it cannot, each file reads as the plain
  // diff lines it always did.
  //
  // Each file is a section of its own, so what comes later (moving file to
  // file, rendering only the files in view) has one place per file to hang on.
  //
  // A large review stays responsive: every file's header is drawn at once,
  // but a file's editors are made only while it is near the view, one file a
  // task, and put away again once it is far from it; the page holds a file's
  // place at the height it will take, or took. Every live editor follows the
  // page's scroll, so how many there are, not how big the review is, is what
  // scrolling costs. Since
  // the editors draw only the rows on screen, highlighting is done only for
  // what can be seen. Generated and very large files start folded with the
  // reason, and are drawn on "Load diff".
  //
  // A large change is found and kept track of file by file in DiffFiles, the
  // frame editing the diff uses too: a filter on the paths, every file
  // folding under its header with its own counts, a step to the next or
  // previous change (also `n` and `p`), and, given `viewed`, a mark on each
  // file the reader has read. Given `collapsed`, every file starts folded
  // unless the diff has only the one; given `stale`, a file changed since it
  // was submitted says so on its header, and no other file says anything.
  //
  // The unchanged lines a hunk leaves out are counted on its header and, given
  // `context`, shown on request: the file is read whole, once, from what the
  // revision pinned, checked against the diff, and the gap written back into
  // the file's diff as context. When it cannot be read, each gap says why.
  //
  // Given `onpick`, a line's number starts a review thread on it, and `marks`
  // marks the lines that carry threads; only lines of the diff itself, not
  // those a gap opened, which the node would not anchor a thread to.
  import { tick, untrack } from 'svelte'

  import Diff from './Diff.svelte'
  import DiffFiles from './DiffFiles.svelte'
  import type { ViewedFiles } from '../lib/viewedFiles.svelte'
  import { splitDiff, type DiffFile } from '../lib/diff'
  import {
    contextMismatch,
    defaultMode,
    estimatedRows,
    expand,
    heavy,
    fileSides,
    noLines,
    parseHunks,
    scrollParent,
    storedMode,
    storedWrap,
    storeMode,
    storeWrap,
    type Context,
    type DiffMode,
    type Expanded,
    type FileSides,
  } from '../lib/diffview'
  import { inDiff, type Numbered } from '../lib/reviewThreads'

  let {
    diff,
    collapsed = false,
    context,
    viewed,
    onpick,
    marks,
    stale = [],
  }: {
    diff: string
    /** Every file starts folded under its header, unless there is only the one. */
    collapsed?: boolean
    /** The files changed since the review was submitted. */
    stale?: string[]
    /** The reader's viewed marks on this revision; absent, files cannot be marked. */
    viewed?: ViewedFiles
    /** Read one file whole on both sides, as the revision pinned it; absent, gaps are only counted. */
    context?: (path: string) => Promise<Context>
    /**
     * Given, each line number of a file, in either mode, is a button that
     * calls it with the line as the node numbers it: how a review thread is
     * started on a line. Only for a diff of the revision itself, whose line
     * numbers are that revision's. Lines a gap opened are not in that diff,
     * so they keep a plain number.
     */
    onpick?: (line: Numbered) => void
    /** How many threads a line carries, to mark it. */
    marks?: (line: Numbered) => number
  } = $props()

  type MergeDiffInstance = {
    rowAt(clientY: number): { line: number; into: number } | null
    rowTop(line: number): number | null
    changeTops(): number[]
  }

  const parsed = $derived(splitDiff(diff))

  /** Each file's pinned sides once read, or why they could not be. */
  let contexts = $state(new Map<string, Context | { why: string }>())
  /** The gaps the reader opened, by file. */
  let opened = $state(new Map<string, ReadonlySet<number>>())
  const NONE: ReadonlySet<number> = new Set()

  // A file's sides are rebuilt only when its own diff, context or open gaps
  // change: new sides remake its editors, and opening a gap in one file must
  // not remake every other.
  type Shown = {
    file: DiffFile
    sides: FileSides
    view: Expanded
    unavailable: string | null
    heavy: ReturnType<typeof heavy>
    /** Which lines a thread can start on: those the diff itself shows, not those an opened gap adds. */
    pickable?: (line: Numbered) => boolean
  }
  const memo = new Map<string, { file: DiffFile; ctx: unknown; open: ReadonlySet<number>; shown: Shown }>()
  function show(file: DiffFile): Shown {
    const ctx = contexts.get(file.path) ?? null
    const open = opened.get(file.path) ?? NONE
    const hit = memo.get(file.path)
    if (hit && hit.file === file && hit.ctx === ctx && hit.open === open) return hit.shown
    const usable = ctx && !('why' in ctx) ? ctx : null
    const view = expand(file, usable, open)
    const shown = {
      file,
      sides: fileSides(view.file),
      view,
      unavailable: ctx && 'why' in ctx ? ctx.why : null,
      heavy: hit?.file === file ? hit.shown.heavy : heavy(file),
      pickable: open.size > 0 ? inDiff(fileSides(file)) : undefined,
    }
    memo.set(file.path, { file, ctx, open, shown })
    return shown
  }
  const files = $derived(parsed.map(show))
  const byPath = $derived(new Map(files.map((f) => [f.file.path, f])))

  /** Read a file's pinned sides, once, and check them against its diff. */
  async function readContext(file: DiffFile): Promise<boolean> {
    if (!context) return false
    const had = contexts.get(file.path)
    if (had) return !('why' in had)
    let got: Context | { why: string }
    try {
      const c = await context(file.path)
      const lines = (t: string | null) => (t === null ? null : t.split('\n'))
      const why =
        c.base === null && c.head === null
          ? 'neither side of the file could be read'
          : contextMismatch(parseHunks(file).hunks, lines(c.base), lines(c.head))
      got = why ? { why } : c
    } catch (e) {
      got = { why: e instanceof Error ? e.message : String(e) }
    }
    contexts = new Map(contexts).set(file.path, got)
    return !('why' in got)
  }

  /** Show the unchanged lines of one gap of a file. */
  async function openGap(file: DiffFile, gap: number) {
    if (!(await readContext(file))) return
    const next = new Set(opened.get(file.path) ?? NONE)
    next.add(gap)
    opened = new Map(opened).set(file.path, next)
  }

  /** Wrap long lines rather than scroll them sideways; the viewer's, remembered like the mode. */
  let wrap = $state(storedWrap())
  function toggleWrap() {
    wrap = !wrap
    storeWrap(wrap)
  }

  let MergeDiff = $state<typeof import('./MergeDiff.svelte').default | null>(null)
  let loadError = $state(false)
  $effect(() => {
    if (MergeDiff || loadError) return
    import('./MergeDiff.svelte').then(
      (m) => (MergeDiff = m.default),
      () => (loadError = true),
    )
  })

  let root = $state<HTMLDivElement>()
  let width = $state(0)
  /** The viewer's own choice, or null while the width decides. */
  let chosen = $state<DiffMode | null>(storedMode())
  /** The mode showing; null until the diff has a width to decide by. */
  const mode = $derived<DiffMode | null>(chosen ?? (width > 0 ? defaultMode(width) : null))

  /**
   * The files open at first: none when collapsed, unless the diff has only
   * the one; otherwise all but the heavy ones and those already viewed on
   * this revision.
   */
  function startOpen(paths: string[]): string[] {
    if (collapsed && paths.length > 1) return []
    return files.filter((f) => (paths.length === 1 || !f.heavy) && viewed?.state(f.file.path) !== 'viewed').map((f) => f.file.path)
  }
  const items = $derived(
    files.map(({ file, sides }) => ({ path: file.path, added: file.added, removed: file.removed, counted: sides.hunks })),
  )
  let frame = $state<ReturnType<typeof DiffFiles>>()

  // --- drawing only what is near --------------------------------------------

  /** Heavy files the reader asked to see anyway. */
  let loaded = $state(new Set<string>())
  /** Files whose editors may be made: they have come near the view. */
  // A record rather than a set: each file's body reads only its own entry, so
  // drawing one file re-renders that file, not every header of the review.
  const drawn = $state<Record<string, true>>({})
  const waiting: string[] = []
  let pumping = false

  /** Make the waiting files' editors one file a task, so the page answers in between. */
  function pump() {
    if (pumping) return
    pumping = true
    const next = () => {
      const path = waiting.shift()
      if (path === undefined) {
        pumping = false
        return
      }
      drawn[path] = true
      setTimeout(next, 0)
    }
    setTimeout(next, 0)
  }

  /**
   * The height each file's editors took when they were last put away, and
   * what they showed then: a height is only good for the same sides, layout
   * and wrapping.
   */
  const heights = new Map<string, { sides: FileSides; mode: DiffMode | null; wrap: boolean; px: number }>()

  // Near is within a screen of the view, so a file is drawn before it is reached.
  const near =
    typeof IntersectionObserver === 'undefined'
      ? null
      : new IntersectionObserver(
          (entries) => {
            for (const e of entries) {
              const el = e.target as HTMLElement
              const path = el.dataset.path
              if (!path) continue
              if (e.isIntersecting) {
                if (!drawn[path] && !waiting.includes(path)) waiting.push(path)
                continue
              }
              const at = waiting.indexOf(path)
              if (at >= 0) waiting.splice(at, 1)
              if (!drawn[path]) continue
              const body = el.querySelector<HTMLElement>(':scope > .body')
              const sides = byPath.get(path)?.sides
              // Fractional: rounding each file's height would add up to a drift.
              const px = body?.getBoundingClientRect().height
              if (px && sides) heights.set(path, { sides, mode, wrap, px })
              delete drawn[path]
            }
            pump()
          },
          { rootMargin: '100% 0px' },
        )
  $effect(() => () => near?.disconnect())

  /** Show a heavy file's diff after all. */
  function load(path: string) {
    loaded = new Set(loaded).add(path)
  }

  // A file's header row, as drawn: what its whole section is before it opens.
  // Measured from the first header on screen; its font and controls decide it.
  let head = $state(31)
  let measured = false

  /**
   * The height a whole file's section will take, for the layout to skip it
   * at before it has ever been drawn: its header, and when it is open, the
   * body its editors take or the line said in their place.
   */
  function intrinsic(path: string, isOpen: boolean): number {
    if (!isOpen) return head
    const f = byPath.get(path)
    if (!f) return head
    const isHeld = f.heavy !== null && !loaded.has(path)
    return head + 4 + (!f.sides.hunks || isHeld ? 40 : placeholder(path, f.sides))
  }

  /** The height a file's editors will take, held until they are made. */
  // A row of the merge view: its 12px text at CodeMirror's line height of 1.4.
  const ROW = 16.8
  function placeholder(path: string, sides: FileSides): number {
    const kept = heights.get(path)
    if (kept && kept.sides === sides && kept.mode === mode && kept.wrap === wrap) return kept.px
    return estimatedRows(sides, mode ?? 'split') * ROW + 8
  }

  // --- finding the way through ----------------------------------------------

  /** Where each change of a drawn file starts on screen; null for one not drawn yet. */
  function changeTops(path: string): number[] | null {
    return views.get(path)?.changeTops() ?? null
  }
  /** A heavy file is stepped through only once the reader has loaded it. */
  const steppable = (path: string) => !heavyOf(path) || loaded.has(path)
  /** A step never opens a heavy file on its own. */
  const opensOnStep = (path: string) => !heavyOf(path)
  const heavyOf = (path: string) => byPath.get(path)?.heavy ?? null

  /** Bring the next (or previous) change into view. */
  export function step(dir: 1 | -1) {
    return frame?.step(dir)
  }

  const views = new Map<string, MergeDiffInstance>()
  const sections = new Map<string, HTMLElement>()
  function register(node: HTMLElement, path: string) {
    sections.set(path, node)
    if (!measured) {
      measured = true
      requestAnimationFrame(() => {
        const h = node.querySelector('summary')?.getBoundingClientRect().height
        if (h) head = h
      })
    }
    if (near) near.observe(node)
    else drawn[path] = true
    return () => {
      near?.unobserve(node)
      sections.delete(path)
    }
  }

  /** What is at the top of the view: a file, and a row of it when its diff is showing. */
  function anchor(): { path: string; line: number | null; at: number; into: number } | null {
    const box = scrollParent(root)
    const at = Math.max(box ? box.getBoundingClientRect().top : 0, root?.getBoundingClientRect().top ?? 0)
    for (const { file } of files) {
      const el = sections.get(file.path)
      if (!el) continue
      const r = el.getBoundingClientRect()
      if (r.bottom <= at) continue
      const row = r.top < at ? (views.get(file.path)?.rowAt(at) ?? null) : null
      return row
        ? { path: file.path, line: row.line, at, into: row.into }
        : { path: file.path, line: null, at, into: at - r.top }
    }
    return null
  }

  /** Scroll so the anchor is back where it was. */
  function restore(a: NonNullable<ReturnType<typeof anchor>>) {
    const el = sections.get(a.path)
    if (!el) return
    const top = a.line !== null ? views.get(a.path)?.rowTop(a.line) : null
    const now = top != null ? top + a.into : el.getBoundingClientRect().top + a.into
    const by = now - a.at
    if (Math.abs(by) < 1) return
    const box = scrollParent(root)
    if (box) box.scrollTop += by
    else window.scrollBy(0, by)
  }

  /** Change the mode for good: this viewer's choice from now on. */
  function choose(next: DiffMode) {
    storeMode(next)
    chosen = next
  }

  // Whatever changes the mode, a choice or (until there is one) the width,
  // the file and row at the top of the view stay there. The anchor is taken
  // before the editors are replaced and put back once the new ones are laid
  // out; they measure their rows a frame after they are made, and rows above
  // the anchor may change height as they do, so it is put back twice.
  let shown: DiffMode | null = null
  $effect.pre(() => {
    const m = mode
    if (m === shown) return
    const was = shown
    shown = m
    if (was === null || m === null) return
    const a = untrack(anchor)
    if (!a) return
    void (async () => {
      await tick()
      for (let i = 0; i < 2; i++) {
        await new Promise((r) => requestAnimationFrame(r))
        restore(a)
      }
    })()
  })

  /** Open a file and bring its header to the top. */
  export function showFile(path: string) {
    return frame?.showFile(path)
  }
</script>

<DiffFiles
  bind:this={frame}
  bind:root
  bind:width
  class="review-diff"
  {items}
  {viewed}
  {startOpen}
  {changeTops}
  {steppable}
  {opensOnStep}
  onsection={register}
  {intrinsic}
>
  {#snippet tools()}
    <div class="modes" role="radiogroup" aria-label="Diff layout">
      <button
        type="button"
        role="radio"
        aria-checked={mode === 'split'}
        class:on={mode === 'split'}
        onclick={() => choose('split')}>Side by side</button
      >
      <button
        type="button"
        role="radio"
        aria-checked={mode === 'unified'}
        class:on={mode === 'unified'}
        onclick={() => choose('unified')}>Unified</button
      >
    </div>
    <button type="button" class="wrap" aria-pressed={wrap} class:on={wrap} onclick={toggleWrap}>Wrap lines</button>
  {/snippet}

  {#snippet chips(path: string)}
    {@const f = byPath.get(path)}
    {#if f}
      {#each f.sides.notes as n (n)}<span class="note">{n === 'binary' ? 'binary file' : n}</span>{/each}
      {#if f.heavy}<span class="heavy-note">{f.heavy.kind === 'generated' ? 'generated' : 'large'}</span>{/if}
    {/if}
    {#if stale.includes(path)}
      <span class="chip warn" title="this file changed in the worktree after the review was submitted">changed since submit</span>
    {/if}
  {/snippet}

  {#snippet body(path: string)}
    {@const f = byPath.get(path)}
    {#if f}
      {@const { file, sides, view, unavailable, heavy: h, pickable } = f}
      {@const held = h !== null && !loaded.has(path)}
      {#if !sides.hunks}
        <div class="empty">{noLines(sides)}</div>
      {:else if held}
        <div class="held">
          <span>{h?.why}: folded so the rest of the review stays quick.</span>
          <button type="button" onclick={() => load(path)}>Load diff</button>
        </div>
      {:else if !drawn[path]}
        <div class="pending" style:height="{placeholder(path, sides)}px"></div>
      {:else if MergeDiff && mode}
        <MergeDiff
          {sides}
          {mode}
          {wrap}
          gaps={view.gaps}
          tail={view.tail}
          {unavailable}
          onexpand={context ? (gap: number) => openGap(file, gap) : undefined}
          {onpick}
          {marks}
          {pickable}
          bind:this={
            () => views.get(path),
            (v) => {
              if (v) views.set(path, v)
              else views.delete(path)
            }
          }
        />
      {:else}
        <Diff diff={file.lines.join('\n')} {onpick} {marks} />
      {/if}
    {/if}
  {/snippet}
</DiffFiles>

<style>
  .wrap {
    font: 12px var(--mono);
    padding: 4px 10px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--s1);
    color: var(--dim);
    cursor: pointer;
  }
  .wrap.on {
    color: var(--ink);
    background: var(--s3);
  }
  .modes {
    display: inline-flex;
    border: 1px solid var(--rule);
    border-radius: 4px;
    overflow: hidden;
  }
  .modes button {
    font: 12px var(--mono);
    padding: 4px 10px;
    border: none;
    background: var(--s1);
    color: var(--dim);
    cursor: pointer;
  }
  .modes button + button {
    border-left: 1px solid var(--rule);
  }
  .modes button.on {
    color: var(--ink);
    background: var(--s3);
  }
  .note {
    color: var(--dim);
    white-space: nowrap;
  }
  .heavy-note {
    color: var(--wait);
    white-space: nowrap;
    font-size: 11.5px;
  }
  .held {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 12px;
    align-items: center;
    padding: 10px 12px;
    background: var(--s2);
    border-radius: 4px;
    color: var(--dim);
    font: 12px var(--mono);
  }
  .held button {
    font: 12px var(--mono);
    padding: 3px 10px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--s1);
    color: var(--ink);
    cursor: pointer;
  }
  .pending {
    background: var(--s2);
    border-radius: 4px;
  }
  .empty {
    padding: 10px 12px;
    background: var(--s2);
    border-radius: 4px;
    color: var(--dim);
    font: 12px var(--mono);
  }
  .chip.warn {
    white-space: nowrap;
    font-size: 11.5px;
  }
</style>
