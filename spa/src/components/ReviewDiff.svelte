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
  // A large change is found and kept track of file by file: a filter on the
  // paths, every file folding under its header with its own counts, a step
  // to the next or previous change (also `n` and `p`), and, given `viewed`,
  // a mark on each file the reader has read. Viewed is the reader's progress
  // and never approval; the bar says so beside the count.
  //
  // The unchanged lines a hunk leaves out are counted on its header and, given
  // `context`, shown on request: the file is read whole, once, from what the
  // revision pinned, checked against the diff, and the gap written back into
  // the file's diff as context. When it cannot be read, each gap says why.
  import { tick, untrack } from 'svelte'

  import Diff from './Diff.svelte'
  import type { ViewedFiles } from '../lib/viewedFiles.svelte'
  import { splitDiff, type DiffFile } from '../lib/diff'
  import {
    contextMismatch,
    defaultMode,
    expand,
    fileSides,
    noLines,
    parseHunks,
    storedMode,
    storedWrap,
    storeMode,
    storeWrap,
    type Context,
    type DiffMode,
    type Expanded,
    type FileSides,
  } from '../lib/diffview'

  let {
    diff,
    collapsed = false,
    context,
    viewed,
  }: {
    diff: string
    collapsed?: boolean
    /** The reader's viewed marks on this revision; absent, files cannot be marked. */
    viewed?: ViewedFiles
    /** Read one file whole on both sides, as the revision pinned it; absent, gaps are only counted. */
    context?: (path: string) => Promise<Context>
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
  type Shown = { file: DiffFile; sides: FileSides; view: Expanded; unavailable: string | null }
  const memo = new Map<string, { file: DiffFile; ctx: unknown; open: ReadonlySet<number>; shown: Shown }>()
  function show(file: DiffFile): Shown {
    const ctx = contexts.get(file.path) ?? null
    const open = opened.get(file.path) ?? NONE
    const hit = memo.get(file.path)
    if (hit && hit.file === file && hit.ctx === ctx && hit.open === open) return hit.shown
    const usable = ctx && !('why' in ctx) ? ctx : null
    const view = expand(file, usable, open)
    const shown = { file, sides: fileSides(view.file), view, unavailable: ctx && 'why' in ctx ? ctx.why : null }
    memo.set(file.path, { file, ctx, open, shown })
    return shown
  }
  const files = $derived(parsed.map(show))

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

  // Open files, by path. On the phone every file starts folded, since the
  // list decides most reviews; elsewhere every file starts open but the ones
  // already viewed on this revision.
  let open = $state(new Set<string>())
  let seeded = ''
  $effect(() => {
    const key = files.map((f) => f.file.path).join('\n')
    if (key === seeded) return
    seeded = key
    untrack(() => {
      open = collapsed
        ? new Set()
        : new Set(files.map((f) => f.file.path).filter((p) => viewed?.state(p) !== 'viewed'))
    })
  })

  // --- finding the way through ----------------------------------------------

  /** What the reader typed to narrow the files to. */
  let filter = $state('')
  const matches = (path: string) => !filter.trim() || path.toLowerCase().includes(filter.trim().toLowerCase())
  const shownCount = $derived(files.filter((f) => matches(f.file.path)).length)
  const totals = $derived(
    files.reduce((t, f) => ({ added: t.added + f.file.added, removed: t.removed + f.file.removed }), { added: 0, removed: 0 }),
  )

  /** Mark a file viewed and fold it, or take the mark off and open it again. */
  function markViewed(path: string, on: boolean) {
    viewed?.set(path, on)
    toggle(path, !on)
  }

  /** Open or fold every file the filter shows. */
  function foldAll(on: boolean) {
    const next = new Set(open)
    for (const { file } of files) {
      if (!matches(file.path)) continue
      if (on) next.delete(file.path)
      else next.add(file.path)
    }
    open = next
  }

  let bar = $state<HTMLDivElement>()

  /**
   * Where the diff's view starts on screen: below the bar, where the bar
   * sits once it sticks. Until the page has scrolled that far the bar is
   * lower, so its own position would move under every step.
   */
  function viewTop(): number {
    const box = scroller()
    return (box ? box.getBoundingClientRect().top : 0) + (bar?.offsetHeight ?? 0)
  }

  /** Where each change in an open, shown file starts on screen, top to bottom. */
  function changeTops(): { path: string; top: number }[] {
    const tops: { path: string; top: number }[] = []
    for (const { file } of files) {
      if (!open.has(file.path) || !matches(file.path)) continue
      for (const top of views.get(file.path)?.changeTops() ?? []) tops.push({ path: file.path, top })
    }
    return tops
  }

  /** A change brought to just below the bar, with a few rows above it showing. */
  const LEAD = 56
  function scrollTo(top: number) {
    const by = top - (viewTop() + LEAD)
    const box = scroller()
    if (box) box.scrollTop += by
    else window.scrollBy(0, by)
  }

  /**
   * Bring the next (or previous) change into view. Past the last change of
   * the open files, the next shown file that is folded and not yet viewed is
   * opened at its first change.
   */
  export async function step(dir: 1 | -1) {
    const at = viewTop() + LEAD
    const tops = changeTops()
    const target = dir > 0 ? tops.find((t) => t.top > at + 2) : tops.findLast((t) => t.top < at - 2)
    if (target) return scrollTo(target.top)
    if (dir < 0) return
    const below = files.find(({ file }) => {
      const el = sections.get(file.path)
      return (
        el &&
        matches(file.path) &&
        !open.has(file.path) &&
        viewed?.state(file.path) !== 'viewed' &&
        el.getBoundingClientRect().top > viewTop()
      )
    })
    if (!below) return
    toggle(below.file.path, true)
    // Its editors are made a frame or two after it opens.
    for (let i = 0; i < 30; i++) {
      await tick()
      await new Promise((r) => requestAnimationFrame(r))
      const first = views.get(below.file.path)?.changeTops()[0]
      if (first !== undefined) return scrollTo(first)
    }
    sections.get(below.file.path)?.scrollIntoView({ block: 'start' })
  }

  /** `n` and `p` step change to change, unless the reader is typing. */
  function onkey(e: KeyboardEvent) {
    if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey || (e.key !== 'n' && e.key !== 'p')) return
    const t = e.target as HTMLElement | null
    if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return
    if (!root?.isConnected || !files.length) return
    e.preventDefault()
    void step(e.key === 'n' ? 1 : -1)
  }

  const views = new Map<string, MergeDiffInstance>()
  const sections = new Map<string, HTMLElement>()
  function register(node: HTMLElement, path: string) {
    sections.set(path, node)
    return { destroy: () => sections.delete(path) }
  }

  function toggle(path: string, on: boolean) {
    if (on === open.has(path)) return
    const next = new Set(open)
    if (on) next.add(path)
    else next.delete(path)
    open = next
  }

  /** The nearest ancestor that scrolls the diff vertically, or null for the window. */
  function scroller(): HTMLElement | null {
    for (let el = root?.parentElement; el; el = el.parentElement) {
      const y = getComputedStyle(el).overflowY
      if ((y === 'auto' || y === 'scroll') && el.scrollHeight > el.clientHeight) return el
    }
    return null
  }

  /** What is at the top of the view: a file, and a row of it when its diff is showing. */
  function anchor(): { path: string; line: number | null; at: number; into: number } | null {
    const box = scroller()
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
    const box = scroller()
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

  /** Open a file and bring its header to the top: how the file list moves to it. */
  export async function showFile(path: string) {
    toggle(path, true)
    await tick()
    sections.get(path)?.scrollIntoView({ block: 'start', behavior: 'smooth' })
  }

  function stat(added: number, removed: number) {
    return `+${added} −${removed}`
  }
</script>

<svelte:window onkeydown={onkey} />

<div class="review-diff" bind:this={root} bind:clientWidth={width}>
  <div class="bar" bind:this={bar}>
    <div class="find">
      <input
        type="search"
        class="filter"
        placeholder="Filter files"
        aria-label="Filter files by path"
        bind:value={filter}
      />
      {#if filter.trim()}<span class="count">{shownCount} of {files.length}</span>{/if}
      <span class="total" title="Lines added and removed in this diff">{stat(totals.added, totals.removed)}</span>
    </div>
    {#if viewed}
      {@const p = viewed.progress}
      <span class="progress" title="Viewed marks are your own reading progress. They approve nothing.">
        {p.viewed}/{p.of} viewed <span class="not">· not approval</span>
      </span>
    {/if}
    <div class="steps">
      <button type="button" title="Previous change (p)" aria-label="Previous change" onclick={() => step(-1)}>↑</button>
      <button type="button" title="Next change (n)" aria-label="Next change" onclick={() => step(1)}>↓</button>
      <button type="button" title="Fold every file shown" onclick={() => foldAll(true)}>Fold all</button>
      <button type="button" title="Open every file shown" onclick={() => foldAll(false)}>Open all</button>
    </div>
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
  </div>
  {#if filter.trim() && shownCount === 0}
    <div class="empty">No file path contains “{filter.trim()}”.</div>
  {/if}
  {#each files as { file, sides, view, unavailable } (file.path)}
    {@const seen = viewed?.state(file.path) ?? 'unviewed'}
    <details
      class="file"
      class:viewed={seen === 'viewed'}
      hidden={!matches(file.path)}
      data-path={file.path}
      open={open.has(file.path)}
      ontoggle={(e) => toggle(file.path, (e.currentTarget as HTMLDetailsElement).open)}
      use:register={file.path}
    >
      <summary>
        <span class="path">{file.path}</span>
        {#each sides.notes as n (n)}<span class="note">{n === 'binary' ? 'binary file' : n}</span>{/each}
        {#if seen === 'changed'}<span class="changed">changed since you viewed it</span>{/if}
        {#if sides.hunks}<span class="stat"
            ><span class="add">+{file.added}</span> <span class="del">−{file.removed}</span></span
          >{/if}
        {#if viewed?.tracks(file.path)}
          <!-- A click on the mark is the mark's, not the fold's. -->
          <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
          <label class="seen" title="Your own reading progress. Marking a file viewed does not approve it." onclick={(e) => e.stopPropagation()}>
            <input
              type="checkbox"
              checked={seen === 'viewed'}
              onchange={(e) => markViewed(file.path, (e.currentTarget as HTMLInputElement).checked)}
            />
            Viewed
          </label>
        {/if}
      </summary>
      {#if open.has(file.path)}
        <div class="body">
          {#if !sides.hunks}
            <div class="empty">{noLines(sides)}</div>
          {:else if MergeDiff && mode}
            <MergeDiff
              {sides}
              {mode}
              {wrap}
              gaps={view.gaps}
              tail={view.tail}
              {unavailable}
              onexpand={context ? (gap: number) => openGap(file, gap) : undefined}
              bind:this={
                () => views.get(file.path),
                (v) => {
                  if (v) views.set(file.path, v)
                  else views.delete(file.path)
                }
              }
            />
          {:else}
            <Diff diff={file.lines.join('\n')} />
          {/if}
        </div>
      {/if}
    </details>
  {/each}
</div>

<style>
  .review-diff {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  /* The switch stays in reach however far into the diff the reader is. */
  .bar {
    position: sticky;
    top: 0;
    z-index: 2;
    display: flex;
    flex-wrap: wrap;
    gap: 6px 8px;
    align-items: center;
    justify-content: flex-end;
    padding: 4px 0;
    background: var(--bg);
  }
  .find {
    display: flex;
    align-items: center;
    gap: 8px;
    flex: 1 1 220px;
    min-width: 0;
  }
  .filter {
    flex: 1;
    min-width: 0;
    max-width: 320px;
    font: 12px var(--mono);
    padding: 4px 8px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--s1);
    color: var(--ink);
  }
  .count,
  .total,
  .progress {
    font: 12px var(--mono);
    color: var(--dim);
    white-space: nowrap;
  }
  .progress .not {
    color: var(--dim);
    font-style: italic;
  }
  .steps {
    display: inline-flex;
    gap: 4px;
  }
  .steps button {
    font: 12px var(--mono);
    padding: 4px 8px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--s1);
    color: var(--ink);
    cursor: pointer;
  }
  .add {
    color: var(--ok);
  }
  .del {
    color: var(--crit);
  }
  .changed {
    color: var(--wait);
    white-space: nowrap;
    font-size: 11.5px;
  }
  .seen {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--dim);
    white-space: nowrap;
    cursor: pointer;
  }
  .file.viewed summary .path {
    color: var(--dim);
  }
  .file[hidden] {
    display: none;
  }
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
  /* A file scrolled to lands below the sticky switch, not under it. */
  .file {
    scroll-margin-top: 40px;
  }
  summary {
    display: flex;
    gap: 10px;
    align-items: baseline;
    padding: 8px 12px;
    background: var(--s1);
    border-radius: 4px;
    cursor: pointer;
    list-style: none;
    font: 12.5px var(--mono);
    min-width: 0;
  }
  summary::-webkit-details-marker {
    display: none;
  }
  summary::before {
    content: '▸';
    color: var(--dim);
  }
  details[open] summary::before {
    content: '▾';
  }
  .path {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--ink);
  }
  .note {
    color: var(--dim);
    white-space: nowrap;
  }
  .stat {
    color: var(--dim);
    white-space: nowrap;
  }
  .body {
    margin-top: 4px;
    min-width: 0;
  }
  .empty {
    padding: 10px 12px;
    background: var(--s2);
    border-radius: 4px;
    color: var(--dim);
    font: 12px var(--mono);
  }
</style>
