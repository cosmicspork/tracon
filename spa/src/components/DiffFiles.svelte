<script lang="ts" module>
  /** One file as its header reads it. */
  export interface DiffFilesItem {
    path: string
    added: number
    removed: number
    /** It has lines to count; a binary file or a pure rename has none. */
    counted: boolean
  }
</script>

<script lang="ts">
  // The frame a review's files are read and edited in: a bar with a filter on
  // the paths, the totals, a step to the next or previous change (also `n`
  // and `p`), Fold all / Open all and, given `viewed`, the reader's progress;
  // then each file folding under a header of its own with its counts and,
  // given `viewed`, its mark. What a file shows once open is the caller's
  // `body`; what its header says besides is the caller's `chips`. Reading
  // (ReviewDiff) and editing the diff (the review page's editor) use the same
  // frame, so the two cannot drift apart.
  //
  // Viewed is the reader's progress and never approval; the bar says so
  // beside the count.
  import type { Snippet } from 'svelte'
  import { tick, untrack } from 'svelte'

  import { scrollParent } from '../lib/diffview'
  import type { ViewedFiles } from '../lib/viewedFiles.svelte'

  let {
    items,
    viewed,
    startOpen,
    chips,
    body,
    tools,
    changeTops,
    steppable,
    opensOnStep,
    onsection,
    intrinsic,
    class: klass = '',
    root = $bindable(),
    width = $bindable(0),
  }: {
    items: DiffFilesItem[]
    /** The reader's viewed marks on this revision; absent, files cannot be marked. */
    viewed?: ViewedFiles
    /** The files open at first, decided again whenever the list of paths changes. */
    startOpen: (paths: string[]) => Iterable<string>
    /** What a file's header says between its path and its counts. */
    chips?: Snippet<[string]>
    /** An open file's body. */
    body: Snippet<[string]>
    /** More controls for the bar, after Fold all / Open all. */
    tools?: Snippet
    /**
     * Where each change of an open file starts on screen, top to bottom; null
     * while its body has none to say (not drawn yet, or not a diff that knows
     * its changes), when it is stepped to by its header.
     */
    changeTops?: (path: string) => number[] | null
    /** Whether a step may stop in an open file; one held back from drawing is not. */
    steppable?: (path: string) => boolean
    /** Whether a step past the last open change may open this folded file. */
    opensOnStep?: (path: string) => boolean
    /** Each file's section as it is made, for a caller that draws only what is near; returns its cleanup. */
    onsection?: (node: HTMLElement, path: string) => (() => void) | void
    /** The height a file's section will take before it is drawn, for the layout to skip it at. */
    intrinsic?: (path: string, open: boolean) => number
    class?: string
    root?: HTMLDivElement
    width?: number
  } = $props()

  // Open files, by path, seeded by the caller whenever the files change.
  let open = $state(new Set<string>())
  let seeded = ''
  $effect(() => {
    const paths = items.map((f) => f.path)
    const key = paths.join('\n')
    if (key === seeded) return
    seeded = key
    untrack(() => {
      open = new Set(startOpen(paths))
    })
  })

  /** What the reader typed to narrow the files to. */
  let filter = $state('')
  const matches = (path: string) => !filter.trim() || path.toLowerCase().includes(filter.trim().toLowerCase())
  const shownCount = $derived(items.filter((f) => matches(f.path)).length)
  const totals = $derived(
    items.reduce((t, f) => ({ added: t.added + f.added, removed: t.removed + f.removed }), { added: 0, removed: 0 }),
  )

  export function isOpen(path: string): boolean {
    return open.has(path)
  }

  export function toggle(path: string, on: boolean) {
    if (on === open.has(path)) return
    const next = new Set(open)
    if (on) next.add(path)
    else next.delete(path)
    open = next
  }

  /** Mark a file viewed and fold it, or take the mark off and open it again. */
  function markViewed(path: string, on: boolean) {
    viewed?.set(path, on)
    toggle(path, !on)
  }

  /** Open or fold every file the filter shows. */
  function foldAll(on: boolean) {
    const next = new Set(open)
    for (const { path } of items) {
      if (!matches(path)) continue
      if (on) next.delete(path)
      else next.add(path)
    }
    open = next
  }

  let bar = $state<HTMLDivElement>()
  const sections = new Map<string, HTMLElement>()

  /**
   * Where the files' view starts on screen: below the bar, where the bar
   * sits once it sticks. Until the page has scrolled that far the bar is
   * lower, so its own position would move under every step.
   */
  function viewTop(): number {
    const box = scrollParent(root)
    return (box ? box.getBoundingClientRect().top : 0) + (bar?.offsetHeight ?? 0)
  }

  /** Where each change in an open, shown file starts on screen, top to bottom. */
  function allTops(): { path: string; top: number }[] {
    const tops: { path: string; top: number }[] = []
    for (const { path, added, removed } of items) {
      if (!open.has(path) || !matches(path) || steppable?.(path) === false) continue
      const within = changeTops?.(path) ?? null
      if (within === null) {
        // A file whose body cannot say is stepped to by its header.
        const el = sections.get(path)
        if (el && added + removed > 0) tops.push({ path, top: el.getBoundingClientRect().top })
        continue
      }
      for (const top of within) tops.push({ path, top })
    }
    return tops
  }

  /** A change brought to just below the bar, with a few rows above it showing. */
  const LEAD = 56
  function scrollTo(top: number) {
    const by = top - (viewTop() + LEAD)
    const box = scrollParent(root)
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
    const tops = allTops()
    const target = dir > 0 ? tops.find((t) => t.top > at + 2) : tops.findLast((t) => t.top < at - 2)
    if (target) return scrollTo(target.top)
    if (dir < 0) return
    const below = items.find(({ path }) => {
      const el = sections.get(path)
      return (
        el &&
        opensOnStep?.(path) !== false &&
        matches(path) &&
        !open.has(path) &&
        viewed?.state(path) !== 'viewed' &&
        el.getBoundingClientRect().top > viewTop()
      )
    })
    if (!below) return
    toggle(below.path, true)
    // Its body is made a frame or two after it opens.
    for (let i = 0; changeTops && i < 30; i++) {
      await tick()
      await new Promise((r) => requestAnimationFrame(r))
      const first = changeTops(below.path)?.[0]
      if (first !== undefined) return scrollTo(first)
    }
    await tick()
    sections.get(below.path)?.scrollIntoView({ block: 'start' })
  }

  /** `n` and `p` step from change to change, unless the reader is typing. */
  function onkey(e: KeyboardEvent) {
    if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey || (e.key !== 'n' && e.key !== 'p')) return
    const t = e.target as HTMLElement | null
    if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return
    if (!root?.isConnected || !items.length) return
    e.preventDefault()
    void step(e.key === 'n' ? 1 : -1)
  }

  function register(node: HTMLElement, path: string) {
    sections.set(path, node)
    const done = onsection?.(node, path)
    return {
      destroy: () => {
        done?.()
        sections.delete(path)
      },
    }
  }

  /** A file's section, while it is on the page. */
  export function section(path: string): HTMLElement | undefined {
    return sections.get(path)
  }

  /** Open a file and bring its header to the top. */
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

<div class="diff-files {klass}" bind:this={root} bind:clientWidth={width}>
  <div class="bar" bind:this={bar}>
    <div class="find">
      <input
        type="search"
        class="filter"
        placeholder="Filter files"
        aria-label="Filter files by path"
        bind:value={filter}
      />
      {#if filter.trim()}<span class="count">{shownCount} of {items.length}</span>{/if}
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
    {@render tools?.()}
  </div>
  {#if filter.trim() && shownCount === 0}
    <div class="empty">No file path contains “{filter.trim()}”.</div>
  {/if}
  {#each items as { path, added, removed, counted } (path)}
    {@const seen = viewed?.state(path) ?? 'unviewed'}
    <details
      class="file"
      class:viewed={seen === 'viewed'}
      class:lazy={intrinsic !== undefined}
      hidden={!matches(path)}
      data-path={path}
      open={open.has(path)}
      ontoggle={(e) => toggle(path, (e.currentTarget as HTMLDetailsElement).open)}
      use:register={path}
      style:contain-intrinsic-block-size={intrinsic ? `auto ${intrinsic(path, open.has(path))}px` : undefined}
    >
      <summary>
        <span class="path">{path}</span>
        {@render chips?.(path)}
        {#if seen === 'changed'}<span class="changed">changed since you viewed it</span>{/if}
        {#if counted}<span class="stat"><span class="add">+{added}</span> <span class="del">−{removed}</span></span>{/if}
        {#if viewed?.tracks(path)}
          <!-- A click on the mark is the mark's, not the fold's. -->
          <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
          <label class="seen" title="Your own reading progress. Marking a file viewed does not approve it." onclick={(e) => e.stopPropagation()}>
            <input
              type="checkbox"
              checked={seen === 'viewed'}
              onchange={(e) => markViewed(path, (e.currentTarget as HTMLInputElement).checked)}
            />
            Viewed
          </label>
        {/if}
      </summary>
      {#if open.has(path)}
        <div class="body">{@render body(path)}</div>
      {/if}
    </details>
  {/each}
</div>

<style>
  .diff-files {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }
  /* The controls stay in reach however far into the files the reader is. */
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
  /* A file scrolled to lands below the sticky bar, not under it. */
  .file {
    scroll-margin-top: 40px;
  }
  /* A file far from the view is skipped by layout and paint altogether, at
     the size it last had or (inline) the size it will have. */
  .file.lazy {
    content-visibility: auto;
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
