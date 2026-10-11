<script lang="ts">
  import {
    eventLine,
    fallbackLine,
    groupLog,
    policyLine,
    groupRunning,
    groupSummary,
    providerErrorLine,
    repetitionLine,
    sessionImage,
    missingContext,
    missingLine,
    orientationContext,
    orientationLine,
    usageMismatchLine,
    usageUnmeteredLine,
    worktreeLine,
  } from '../lib/log'
  import { clock } from '../lib/clock.svelte'
  import { formatTokens } from '../lib/format'
  import { formatSpan, runFailures, runMs, runRunning, transcript, type CallRow } from '../lib/transcript'
  import { renderMessage } from '../lib/markdown'
  import type { Event } from '../lib/types'

  let {
    events,
    openChunks,
    toolProgress,
    toolOutput = new Map(),
    root = null,
  }: {
    events: Event[]
    openChunks: Map<string, { kind: string; text: string }>
    toolProgress: Map<string, string>
    toolOutput?: Map<string, string>
    /** The session's worktree: paths inside it are shown from it. */
    root?: string | null
  } = $props()

  const entries = $derived(groupLog(events, toolProgress, toolOutput))

  let el = $state<HTMLElement | null>(null)
  let pinned = $state(true)

  // Follow the stream unless the operator has scrolled up to read.
  $effect(() => {
    void entries.length
    void openChunks
    if (pinned && el) el.scrollTop = el.scrollHeight
  })

  function onScroll() {
    if (!el) return
    pinned = el.scrollHeight - el.scrollTop - el.clientHeight < 40
  }

  function text(e: Event): string {
    return typeof e.payload.text === 'string' ? e.payload.text : ''
  }

  function toolLine(call: Event, result: Event | undefined, progress: string | undefined): string {
    const title = (call.payload.title as string) ?? call.ref_id ?? 'tool'
    const status = (result?.payload.status as string) ?? progress ?? 'running'
    const truncated = result?.payload.truncated === true ? ' · output truncated' : ''
    return `${title} · ${status}${truncated}`
  }

  /// "running · 9s", "failed · 1.2s", "0.4s": how a call ended and how long
  /// it took.
  function callMeta(state: CallRow['state'], ms: number | null, note = ''): string {
    const span = ms !== null ? formatSpan(ms) : ''
    return [note, state === 'done' ? '' : state, span].filter(Boolean).join(' · ')
  }

  const GLYPH: Record<CallRow['state'], string> = { done: '✓', failed: '✗', running: '◌' }

  /// One key opens every run on the screen, or folds them all when every one
  /// is already open. Not while typing: the composer is on the same screen.
  function onKey(e: KeyboardEvent) {
    if (e.key !== 'o' || e.metaKey || e.ctrlKey || e.altKey || e.defaultPrevented || !el) return
    const target = e.target as HTMLElement | null
    if (target?.closest('input, textarea, select, [contenteditable=""], [contenteditable="true"]')) return
    const runs = [...el.querySelectorAll<HTMLDetailsElement>('details.run')]
    if (!runs.length) return
    const open = !runs.every((d) => d.open)
    for (const d of runs) d.open = open
  }

  function turnEnd(e: Event): string {
    const usage = e.payload.usage as { total_tokens?: number } | undefined
    const total = usage?.total_tokens
    return total !== undefined
      ? `turn ended · ${formatTokens(total)} tokens`
      : `turn ended · ${e.payload.stop_reason ?? ''}`
  }
</script>

<svelte:window onkeydown={onKey} />

{#snippet callLine(c: CallRow)}
  <details class="call" class:crit={c.state === 'failed'}>
    <summary>
      <!-- One child: on a phone a summary is a flex row. -->
      <span class="line"><span class="st {c.state}">{GLYPH[c.state]}</span>{c.text}{#if callMeta(c.state, c.ms, c.note)}<span class="meta">{' · '}{callMeta(c.state, c.ms, c.note)}</span>{/if}</span>
    </summary>
    <div class="out">{c.output || 'no output recorded'}{c.truncated ? '\n… the node keeps only the start of a long output' : ''}</div>
  </details>
  {#if c.error}<div class="err">{c.error}</div>{/if}
  {#if c.tail}<div class="tail">{c.tail}</div>{/if}
{/snippet}

<div class="log" bind:this={el} onscroll={onScroll}>
  {#each entries as entry, i (entry.event?.seq ?? `tools-${i}`)}
    {#if entry.kind === 'leaf'}
      {@const e = entry.event!}
      {#if e.kind === 'user_prompt'}
        <div class="you"><span class="prompt">›</span><div class="md">{@html renderMessage(text(e))}</div></div>
      {:else if e.kind === 'message'}
        <div class="msg md">{@html renderMessage(text(e))}</div>
      {:else if e.kind === 'thought'}
        <details class="fold">
          <summary>thought</summary>
          <div>{text(e)}</div>
        </details>
      {:else if e.kind === 'permission_request'}
        <div class="mark wait">permission · {e.payload.title}</div>
      {:else if e.kind === 'permission_answer'}
        <div class="mark">
          answered: {e.payload.option_id}{e.payload.arguments ? ' (edited)' : ''}{e.payload.grant
            ? ` · for this session: ${e.payload.grant}`
            : ''}
        </div>
      {:else if e.kind === 'permission_expired'}
        <div class="mark crit">{e.payload.reason ?? 'denied: unanswered'}</div>
      {:else if e.kind === 'turn_end'}
        <div class="mark">{turnEnd(e)}</div>
      {:else if e.kind === 'worktree'}
        <div class="sys">{worktreeLine(e.payload)}</div>
      {:else if e.kind === 'session_started'}
        <div class="sys">harness started · {e.payload.model}{e.payload.phase ? ` · ${e.payload.phase}` : ''}{e.payload.policy_version != null ? ` · policy v${e.payload.policy_version}` : ''}{sessionImage(e.payload)}</div>
      {:else if e.kind === 'egress_refused'}
        <div class="mark wait">{e.payload.host} · not reachable from a session{e.payload.approval_id ? '; the operator was asked' : "; add it to this repository's egress"}</div>
      {:else if e.kind === 'repo_image'}
        <div class="sys">building the repository's image · the harness starts in it when it is ready, which takes minutes the first time</div>
      {:else if e.kind === 'check_started'}
        <div class="mark">checks started · {((e.payload.commands as string[]) ?? []).join(' · ')}</div>
      {:else if e.kind === 'check_prepared'}
        <div class={e.payload.ok ? 'mark' : 'mark crit'}>dependencies {e.payload.ok ? 'prepared' : 'not prepared'} · once for this run · {Math.round(((e.payload.ms as number) ?? 0) / 1000)}s</div>
      {:else if e.kind === 'workspace_prepared' && e.payload.detail}
        <!-- A failed preparation keeps what it said behind the line; the
             others are a line from eventLine like any other kind. -->
        <details class="fold" open>
          <summary class="crit">{eventLine(e)?.text}</summary>
          <div>{e.payload.detail}</div>
        </details>
      {:else if e.kind === 'check_result'}
        <details class="fold" open={e.payload.ok !== true}>
          <summary class={e.payload.ok ? '' : 'crit'}>{e.payload.ok ? '✓' : '✗'} {e.payload.command} · exit {e.payload.exit ?? 'none'} · {Math.round(((e.payload.ms as number) ?? 0) / 1000)}s</summary>
          {#if e.payload.failures}
            <div class="failures"><span class="label">what failed</span>{'\n'}{e.payload.failures}</div>
            <div><span class="label">end of the output</span>{'\n'}{e.payload.tail || '(no output)'}</div>
          {:else}
            <div>{e.payload.tail || '(no output)'}</div>
          {/if}
        </details>
      {:else if e.kind === 'check_cancelled'}
        <div class="mark crit">checks cancelled · {e.payload.reason} · nothing was verified</div>
      {:else if e.kind === 'check_not_runnable'}
        <div class="mark crit">
          not runnable · {e.payload.command} · {e.payload.missing_tool ?? 'its command'} is not in
          {e.payload.image} ({e.payload.image_source}) · the environment, not the change
        </div>
      {:else if e.kind === 'repetition'}
        <div class="mark wait">{repetitionLine(e.payload)}</div>
      {:else if e.kind === 'late_refused'}
        <div class="sys">refused · {e.payload.what} arrived after the session was {e.payload.state}</div>
      {:else if e.kind === 'review_rejected'}
        <div class="mark crit">submission refused · {e.payload.reason}</div>
      {:else if e.kind === 'plan_artifact'}
        <div class="mark ok">plan written · <a href="/docs/{e.payload.channel ?? ''}/{e.payload.slug}">{e.payload.slug}</a> · ends the phase</div>
      {:else if e.kind === 'work_shown'}
        <div class="mark ok">work shown · {e.payload.title} · at {String(e.payload.head_sha ?? '').slice(0, 12)}{e.payload.files ? ` · ${e.payload.files} ${e.payload.files === 1 ? 'file' : 'files'}` : ''} · in tracon only</div>
      {:else if e.kind === 'forge_follow'}
        {@const changes = Array.isArray(e.payload.changes) ? (e.payload.changes as string[]) : []}
        {@const url = typeof e.payload.url === 'string' ? e.payload.url : null}
        {#if changes.length}
          <div class="mark">on the forge · {changes.join(' · ')}{#if url}{' · '}<a href={url} target="_blank" rel="noopener">open</a>{/if}</div>
        {/if}
      {:else if e.kind === 'pipeline_follow'}
        {@const changes = Array.isArray(e.payload.changes) ? (e.payload.changes as string[]) : []}
        {@const url = typeof e.payload.url === 'string' ? e.payload.url : null}
        {#if changes.length}
          <div class="mark" class:crit={e.payload.status === 'failed'}>{e.payload.provider === 'github' ? 'run' : 'pipeline'} {e.payload.pipeline_id} · {changes.join(' · ')}{#if url}{' · '}<a href={url} target="_blank" rel="noopener">open</a>{/if}</div>
        {/if}
      {:else if e.kind === 'work_closed'}
        <div class="mark">work closed{e.payload.summary ? ` · ${e.payload.summary}` : ''}</div>
      {:else if e.kind === 'review_verdict'}
        <div class="mark ok">verdict · {e.payload.verdict} · {e.payload.summary}</div>
      {:else if e.kind === 'provider_error'}
        {#if typeof e.payload.frame === 'string' && e.payload.frame}
          <!-- A retry that named no cause keeps what the harness sent. -->
          <details class="fold">
            <summary class="wait">{providerErrorLine(e.payload)}</summary>
            <div class="raw">{e.payload.frame}</div>
          </details>
        {:else}
          <div class="mark wait">{providerErrorLine(e.payload)}</div>
        {/if}
      {:else if e.kind === 'gateway_refused'}
        <div class="mark crit">model call refused · {e.payload.provider} · {e.payload.reason}</div>
      {:else if e.kind === 'workspace_changed'}
        <div class="mark wait">workspace changed by the harness · {e.payload.action} · anything bound to the tree as it was no longer holds</div>
      {:else if e.kind === 'ceiling'}
        <div class="mark crit">channel at its daily ceiling · {e.payload.usage_today} of {e.payload.ceiling} tokens · model calls refused</div>
      {:else if e.kind === 'usage_mismatch'}
        <div class="mark wait">{usageMismatchLine(e.payload)}</div>
      {:else if e.kind === 'usage_unmetered'}
        <div class="mark wait">{usageUnmeteredLine(e.payload)}</div>
      {:else if e.kind === 'orientation'}
        {@const missing = missingContext(e.payload)}
        {@const context = orientationContext(e.payload)}
        <details class="fold">
          <summary class:wait={missing.length > 0}>{orientationLine(e.payload)}</summary>
          <div>
            {#if context && context.changes.length > 0}
              <!-- What this attempt was given that the previous one was not,
                   or the other way round. -->
              <ul class="missing">
                {#each context.changes as c, i (i)}
                  <li>context: {c.says}</li>
                {/each}
              </ul>
            {/if}
            {#if missing.length > 0}
              <!-- Named, not flagged: the operator can see whether the guide
                   the session needed was one of the ones it did not get. -->
              <ul class="missing">
                {#each missing as m (m.what)}
                  <li>{missingLine(m)}</li>
                {/each}
              </ul>
            {/if}
            {e.payload.text}
          </div>
        </details>
      {:else if e.kind === 'host_suspended'}
        <div class="mark wait">
          the machine slept for {Math.max(1, Math.round(Number(e.payload.asleep_ms) / 60_000))} min{e.payload.turn_active
            ? ' in the middle of a turn'
            : ''} · waiting cards kept their time{e.payload.inhibitor_held ? ' · it was asked to stay awake' : ''}
        </div>
      {:else if e.kind === 'state'}
        <div class="sys">→ {e.payload.state}</div>
      {:else if e.kind === 'error'}
        <div class="mark crit">{e.payload.error ?? JSON.stringify(e.payload)}</div>
      {:else if e.kind === 'tool_result'}
        <div class="sys">{toolLine(e, e, undefined)}</div>
      {:else if e.kind === 'policy_denied'}
        <div class="mark crit">{policyLine(e)}</div>
      {:else if e.kind === 'policy_allowed'}
        <div class="mark wait">{policyLine(e)}</div>
      {:else}
        <!-- groupLog has already dropped the silent kinds. -->
        {@const line = eventLine(e) ?? fallbackLine(e)}
        {@const external = line.href?.startsWith('http') ?? false}
        <div class={line.tone === 'sys' || line.tone === 'mark' ? line.tone : `mark ${line.tone}`}>
          {line.text}{#if line.href}{' · '}<a href={line.href} target={external ? '_blank' : undefined} rel={external ? 'noopener' : undefined}>{line.link ?? 'open'}</a>{/if}
        </div>
      {/if}
    {:else}
      {@const tools = entry.tools!}
      {@const live = groupRunning(tools)}
      <!-- Only a run still going reads the clock, so a finished log does not
           re-render every second. -->
      {@const now = live ? clock.now : 0}
      {@const rows = transcript(tools, root, now)}
      {#if tools.length === 1 && rows[0].kind === 'call'}
        <div class="calls">{@render callLine(rows[0].call)}</div>
      {:else}
        {@const ms = runMs(tools, now)}
        {@const failures = runFailures(tools, root)}
        <!-- Folded unless the operator opens it: binding `open` to the run
             still going sprang it open at every call and shut at every result,
             and undid the operator's own choice each time. -->
        <details class="fold tools run">
          <summary title="o opens or folds every run">{groupSummary(tools)}{ms !== null ? ` · ${formatSpan(ms)}` : ''}</summary>
          <div class="calls">
            {#each rows as row, j (j)}
              {#if row.kind === 'call'}
                {@render callLine(row.call)}
              {:else}
                <details class="call reads" class:crit={row.state === 'failed'}>
                  <summary>
                    <span class="line"><span class="st {row.state}">{GLYPH[row.state]}</span>{row.text}{#if callMeta(row.state, row.ms)}<span class="meta">{' · '}{callMeta(row.state, row.ms)}</span>{/if}</span>
                  </summary>
                  <div class="calls">
                    {#each row.calls as c (c.entry.call.seq)}
                      {@render callLine(c)}
                    {/each}
                  </div>
                </details>
              {/if}
            {/each}
          </div>
        </details>
        {#if failures.length}
          <!-- A failure is never behind a click: folded, the run still names it. -->
          <div class="fails">
            {#each failures as f, j (j)}
              <div><span class="st failed">{GLYPH.failed}</span>{f.text}{f.error ? ` · ${f.error}` : ''}</div>
            {/each}
          </div>
        {/if}
        {#if live}
          <!-- What is running now, under the fold; hidden once the run is
               open, where the same call is already in the list. -->
          <div class="calls running">
            {#each runRunning(tools, root, now) as c (c.entry.call.seq)}
              {@render callLine(c)}
            {/each}
          </div>
        {/if}
      {/if}
    {/if}
  {/each}
  {#each [...openChunks.values()] as chunk, i (i)}
    {#if chunk.kind === 'message'}
      <div class="msg live">{chunk.text}<span class="cursor">▍</span></div>
    {:else}
      <div class="sys live">{chunk.text}</div>
    {/if}
  {/each}
</div>

<style>
  .log {
    font: 12.5px/1.6 var(--mono);
    display: flex;
    flex-direction: column;
    gap: 6px;
    background: var(--s1);
    border-radius: 4px;
    padding: 14px 16px;
    overflow-y: auto;
    flex: 1;
    min-height: 200px;
  }
  .you {
    color: var(--acc);
    display: flex;
    gap: 1ch;
  }
  .you .md {
    min-width: 0;
    flex: 1;
  }
  .msg {
    color: var(--ink);
  }
  /* A finished message is Markdown; one still arriving is shown as it streams. */
  .msg.live {
    white-space: pre-wrap;
  }
  .md {
    overflow-wrap: anywhere;
  }
  .md > :global(:first-child) {
    margin-top: 0;
  }
  .md > :global(:last-child) {
    margin-bottom: 0;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol),
  .md :global(blockquote),
  .md :global(pre),
  .md :global(table) {
    margin: 4px 0;
  }
  .md :global(ul),
  .md :global(ol) {
    padding-left: 2.5ch;
  }
  .md :global(h1),
  .md :global(h2),
  .md :global(h3),
  .md :global(h4) {
    font-size: 1em;
    font-weight: 600;
    margin: 10px 0 4px;
  }
  .md :global(h1),
  .md :global(h2) {
    font-size: 1.08em;
  }
  .md :global(code) {
    background: var(--s2);
    border-radius: 3px;
    padding: 0 3px;
  }
  .md :global(pre) {
    background: var(--s2);
    border-radius: 4px;
    padding: 8px 10px;
    overflow-x: auto;
  }
  .md :global(pre code) {
    background: none;
    padding: 0;
    overflow-wrap: normal;
  }
  /* A wide table scrolls on its own rather than widening the log. */
  .md :global(table) {
    display: block;
    max-width: 100%;
    overflow-x: auto;
    border-collapse: collapse;
  }
  .md :global(th),
  .md :global(td) {
    border: 1px solid var(--rule);
    padding: 2px 8px;
    text-align: left;
  }
  .md :global(blockquote) {
    border-left: 2px solid var(--rule);
    padding-left: 1.5ch;
    color: var(--ink2);
  }
  .md :global(hr) {
    border: 0;
    border-top: 1px solid var(--rule);
  }
  .md :global(a) {
    color: var(--acc);
  }
  .md :global(.no-fetch) {
    color: var(--dim);
  }
  .sys {
    color: var(--dim);
  }
  .mark {
    color: var(--ink2);
  }
  /* A path, a URL or an id is one unbroken word; it wraps, not the page. */
  .sys,
  .mark {
    overflow-wrap: anywhere;
  }
  .mark.wait {
    color: var(--wait);
  }
  .mark.crit,
  .tool.crit,
  .fold > summary.crit {
    color: var(--crit);
  }
  .fold > summary.wait {
    color: var(--wait);
  }
  /* What the session was not told, named above the text it was told. */
  .missing {
    margin: 0 0 10px;
    padding-left: 16px;
    color: var(--wait);
    font-size: 12px;
  }
  .mark.ok {
    color: var(--ok);
  }
  .fold > summary {
    color: var(--dim);
    font-size: 12px;
    cursor: pointer;
    list-style: none;
  }
  .fold > summary::-webkit-details-marker {
    display: none;
  }
  .fold > summary::before {
    content: '▸';
    display: inline-block;
    width: 12px;
  }
  .fold[open] > summary::before {
    content: '▾';
  }
  .fold[open] > summary {
    color: var(--ink2);
  }
  .fold > div {
    padding: 4px 0 2px 18px;
    border-left: 1px solid var(--rule);
    margin: 4px 0 0 4px;
    color: var(--dim);
    white-space: pre-wrap;
  }
  /* A harness frame is one long line of JSON. */
  .fold > div.raw {
    overflow-wrap: anywhere;
  }
  /* The lines that say why a check failed, read out of the whole output. */
  .fold > div.failures {
    border-left-color: var(--crit);
    color: var(--ink2);
  }
  .fold .label {
    color: var(--dim);
    font-size: 11px;
  }
  /* The transcript: a line per call, each opening to its output. */
  .log .calls {
    display: flex;
    flex-direction: column;
    gap: 1px;
    white-space: normal;
  }
  .call summary {
    cursor: pointer;
    list-style: none;
    color: var(--ink2);
    overflow-wrap: anywhere;
  }
  .call summary::-webkit-details-marker {
    display: none;
  }
  .call.crit > summary {
    color: var(--crit);
  }
  .st {
    display: inline-block;
    width: 2ch;
    color: var(--dim);
  }
  .st.failed {
    color: var(--crit);
  }
  .st.running {
    color: var(--wait);
  }
  .meta {
    color: var(--dim);
  }
  .call > .out {
    margin: 2px 0 6px 2ch;
    padding: 4px 0 4px 10px;
    border-left: 1px solid var(--rule);
    color: var(--dim);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 24em;
    overflow-y: auto;
  }
  .call.reads > .calls {
    margin-left: 2ch;
  }
  /* A failed call's first error line, and a running call's last output. */
  .err,
  .tail,
  .fails {
    margin-left: 2ch;
    font-size: 12px;
    overflow-wrap: anywhere;
  }
  .err {
    color: var(--crit);
  }
  .tail {
    color: var(--dim);
    white-space: pre-wrap;
  }
  .fails {
    color: var(--crit);
    margin-left: 18px;
  }
  /* Lined up with the failures above it: a call line's summary brings its
     own indent. */
  .running {
    margin-left: 4px;
  }
  /* Opened, the call shows its whole output; the run, each failure and each
     running call in its place. */
  .call[open] + .err,
  .run[open] + .fails,
  .run[open] + .running,
  .run[open] + .fails + .running {
    display: none;
  }
  .cursor {
    color: var(--ink2);
    animation: blink 1s steps(1) infinite;
  }
  @keyframes blink {
    50% {
      opacity: 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .cursor {
      animation: none;
    }
  }
</style>
