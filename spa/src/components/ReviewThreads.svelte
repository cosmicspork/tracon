<script lang="ts">
  // The operator's review threads, and the composer that opens one. A thread
  // is general, on a file, or on a line or range of the revision on screen;
  // a line number picked in the diff fills the composer's anchor. Each
  // thread stays on the revision it was written on: one whose lines a later
  // revision changed reads outdated and still shows the lines it meant.
  // Unused, the section is one line; with threads it folds to a count, open
  // while any of them asks for something.
  import { api } from '../lib/api'
  import Help from './Help.svelte'
  import {
    STATE_LABEL,
    anchorProblem,
    createBody,
    threadsStartOpen,
    whereLabel,
    type Anchor,
    type ReviewThread,
    type ThreadKind,
  } from '../lib/reviewThreads'

  let {
    reviewId,
    revisionId,
    revisionNumber,
    paths,
    threads,
    anchor = $bindable(),
    composing = $bindable(false),
    blocked,
    onthreads,
  }: {
    reviewId: string
    /** The revision on screen; a new thread is bound to it. */
    revisionId: string | null | undefined
    /** Its number, from 1, for the labels. */
    revisionNumber: number | null
    /** The files this revision's diff changes. */
    paths: string[]
    threads: ReviewThread[]
    /** Where the next thread goes; a pick in the diff sets it. */
    anchor: Anchor
    /** The composer is open; `+ Thread` or a pick in the diff opens it. */
    composing?: boolean
    /** Why threads cannot be changed from here, or null when they can. */
    blocked: string | null
    onthreads: (threads: ReviewThread[]) => void
  } = $props()

  let body = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)
  let showResolved = $state(false)
  /** The thread whose reply box is open, and what it holds. */
  let replying = $state<string | null>(null)
  let reply = $state('')

  const open = $derived(threads.filter((t) => t.state !== 'resolved'))
  const resolved = $derived(threads.filter((t) => t.state === 'resolved'))
  /** The operator's fold, once they have toggled it; until then the threads decide. */
  let userOpen = $state<boolean | null>(null)
  const expanded = $derived(userOpen ?? threadsStartOpen(threads))
  const problem = $derived(anchorProblem(anchor, body))

  function setKind(kind: ThreadKind) {
    anchor =
      kind === 'general'
        ? { kind }
        : kind === 'file'
          ? { kind, path: anchor.path ?? paths[0], side: anchor.side ?? 'new' }
          : {
              kind,
              path: anchor.path ?? paths[0],
              side: anchor.side ?? 'new',
              start_line: anchor.start_line ?? 1,
              end_line: anchor.end_line ?? anchor.start_line ?? 1,
            }
  }

  function lineInput(which: 'start_line' | 'end_line', value: string) {
    const n = Number.parseInt(value, 10)
    anchor = { ...anchor, [which]: Number.isFinite(n) ? n : undefined }
  }

  async function act(work: () => Promise<{ threads: ReviewThread[] }>): Promise<boolean> {
    busy = true
    error = null
    try {
      onthreads((await work()).threads)
      return true
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      return false
    } finally {
      busy = false
    }
  }

  async function add() {
    if (problem) return
    if (await act(() => api.createReviewThread(reviewId, createBody(anchor, body, revisionId)))) {
      body = ''
      anchor = { kind: 'general' }
      composing = false
    }
  }

  function compose() {
    anchor = { kind: 'general' }
    composing = true
  }

  function cancel() {
    body = ''
    anchor = { kind: 'general' }
    composing = false
  }

  async function sendReply(thread: string) {
    if (!reply.trim()) return
    if (await act(() => api.replyToReviewThread(reviewId, thread, reply.trim()))) {
      reply = ''
      replying = null
    }
  }
</script>

<div class="h4 head">
  {#if threads.length}
    <button class="fold" aria-expanded={expanded} onclick={() => (userOpen = !expanded)}
      >Threads <b>· {open.length} open · {resolved.length} resolved</b></button
    >
  {:else}
    <span>Threads <b>· none</b></span>
  {/if}
  {#if !blocked && !composing}
    <button class="lnk add" onclick={compose}>+ Thread</button>
  {/if}
  <Help label="About threads"
    >A thread is on the whole change, a file, or lines of revision {revisionNumber ?? 1}, and the agent reads it. Pick a
    line number in the diff to start one there; pick another to make it a range. Request changes sends the open ones.</Help
  >
</div>

{#if error}<div class="banner crit">refused <b>· {error}</b></div>{/if}

{#if composing && !blocked}
  <div class="composer">
    <div class="row">
      <label>
        On
        <select value={anchor.kind} disabled={busy} onchange={(e) => setKind(e.currentTarget.value as ThreadKind)}>
          <option value="general">the whole change</option>
          <option value="file" disabled={!paths.length}>a file</option>
          <option value="line" disabled={!paths.length}>lines</option>
        </select>
      </label>
      {#if anchor.kind !== 'general'}
        <select
          aria-label="file"
          value={anchor.path}
          disabled={busy}
          onchange={(e) => (anchor = { ...anchor, path: e.currentTarget.value })}
        >
          {#each paths as p (p)}<option value={p}>{p}</option>{/each}
        </select>
        <select
          aria-label="side"
          value={anchor.side ?? 'new'}
          disabled={busy}
          onchange={(e) => (anchor = { ...anchor, side: e.currentTarget.value as 'old' | 'new' })}
        >
          <option value="new">the change</option>
          <option value="old">the base</option>
        </select>
      {/if}
      {#if anchor.kind === 'line'}
        <label>
          lines
          <input
            class="n"
            type="number"
            min="1"
            value={anchor.start_line ?? ''}
            disabled={busy}
            oninput={(e) => lineInput('start_line', e.currentTarget.value)}
          />
          –
          <input
            class="n"
            type="number"
            min="1"
            value={anchor.end_line ?? ''}
            disabled={busy}
            oninput={(e) => lineInput('end_line', e.currentTarget.value)}
          />
        </label>
      {/if}
    </div>
    <textarea
      class="grow compact"
      aria-label="new thread"
      placeholder={`What should change${anchor.kind === 'general' ? '' : ` at ${whereLabel(anchor)}`}? The agent reads this.`}
      bind:value={body}
      disabled={busy}
    ></textarea>
    <div class="row">
      <button class="btn" disabled={busy || problem !== null} onclick={add}>Add thread</button>
      <button class="btn" disabled={busy} onclick={cancel}>Cancel</button>
      {#if problem && body.trim()}<span class="note dim">{problem}</span>{/if}
    </div>
  </div>
{:else if blocked && expanded}
  <p class="note dim">{blocked}</p>
{/if}

{#snippet card(t: ReviewThread)}
  <article class="thread" class:done={t.state === 'resolved'} data-thread={t.id}>
    <header>
      <span class="where">{whereLabel(t)}</span>
      <span
        class="chip"
        class:warn={t.state === 'unresolved' || t.state === 'outdated'}
        class:off={t.state === 'resolved'}>{STATE_LABEL[t.state]}</span
      >
      <span class="tag">revision {t.revision ?? '?'}</span>
      {#if t.addressed_revision}<span class="tag">addressed in {t.addressed_revision}</span>{/if}
    </header>
    {#if t.outdated}
      <p class="note dim">
        Its lines changed after revision {t.revision ?? '?'}. It stays on what it was written on{t.lines ? ', below' : ''}.
      </p>
    {/if}
    {#if t.lines}<pre class="lines">{t.lines}</pre>{/if}
    <ol class="comments">
      {#each t.comments as c, i (i)}
        <li class={c.author}>
          <span class="who">{c.author === 'agent' ? 'Agent' : 'You'}{c.revision ? ` · revision ${c.revision}` : ''}</span>
          <div class="body">{c.body}</div>
        </li>
      {/each}
    </ol>
    {#if !blocked}
      {#if replying === t.id}
        <textarea class="grow compact" aria-label="reply" bind:value={reply} disabled={busy}></textarea>
        <div class="row">
          <button class="btn" disabled={busy || !reply.trim()} onclick={() => sendReply(t.id)}>Reply</button>
          <button class="btn" disabled={busy} onclick={() => (replying = null)}>Back</button>
        </div>
      {:else}
        <div class="row">
          <button class="lnk" disabled={busy} onclick={() => ((replying = t.id), (reply = ''))}>Reply</button>
          {#if t.state === 'resolved'}
            <button class="lnk" disabled={busy} onclick={() => act(() => api.reopenReviewThread(reviewId, t.id))}>Reopen</button>
          {:else}
            {#if t.state === 'addressed'}
              <button class="lnk" disabled={busy} onclick={() => act(() => api.reopenReviewThread(reviewId, t.id))}
                >Not yet: reopen</button
              >
            {/if}
            <button class="lnk" disabled={busy} onclick={() => act(() => api.resolveReviewThread(reviewId, t.id))}>Resolve</button>
          {/if}
        </div>
      {/if}
    {/if}
  </article>
{/snippet}

{#if expanded}
  {#each open as t (t.id)}{@render card(t)}{/each}
  {#if resolved.length}
    <button class="lnk" onclick={() => (showResolved = !showResolved)}>
      {showResolved ? 'Hide' : 'Show'} {resolved.length} resolved
    </button>
    {#if showResolved}{#each resolved as t (t.id)}{@render card(t)}{/each}{/if}
  {/if}
{/if}

<style>
  /* Not @tracon/ui's dim .note: these lines keep the colour of the text
     around them. */
  .note {
    color: inherit;
  }
  .head {
    align-items: center;
  }
  .fold {
    font: inherit;
    letter-spacing: inherit;
    color: inherit;
    background: none;
    border: 0;
    padding: 0;
    cursor: pointer;
    text-align: left;
  }
  .fold::before {
    content: '▸ ';
    color: var(--dim);
  }
  .fold[aria-expanded='true']::before {
    content: '▾ ';
  }
  .add {
    font: 12.5px var(--mono);
  }
  .composer,
  .thread {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px 12px;
    background: var(--s1);
    border-radius: 4px;
    margin-bottom: 6px;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }
  .n {
    width: 7ch;
  }
  select,
  input {
    font: 12.5px var(--mono);
    max-width: 100%;
  }
  .thread.done {
    opacity: 0.7;
  }
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 8px;
  }
  .where {
    font: 12.5px var(--mono);
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  .lines {
    margin: 0;
    padding: 6px 10px;
    background: var(--s2);
    border-radius: 4px;
    font: 12px/1.5 var(--mono);
    overflow-x: auto;
  }
  .comments {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .who {
    font: 11px var(--mono);
    color: var(--dim);
  }
  .body {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .agent .body {
    padding-left: 8px;
    border-left: 2px solid var(--rule);
  }
</style>
