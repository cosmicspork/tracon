<script lang="ts">
  import { api, ApiError } from '../lib/api'
  import {
    approvalArguments,
    approvalFields,
    asArguments,
    CALLER_ONLY,
    changedArguments,
    editProblems,
    grantsAccess,
    outcome,
    problemsByField,
    resultRows,
    schemaFor,
    type ApprovalDetails,
  } from '../lib/approval'
  import { autogrow } from '../lib/autogrow'
  import { clock } from '../lib/clock.svelte'
  import { formatAge, formatExpiry } from '../lib/format'
  import { isNotFound } from '../lib/load'
  import { renderJiraWiki } from '../lib/jira-wiki'
  import { render } from '../lib/markdown'
  import { router } from '../lib/router.svelte'
  import { formValues, type Field, type FieldValue } from '../lib/schema-form'
  import { store } from '../lib/store.svelte'
  import { diffArguments } from '../lib/text-diff'
  import { leaveAfterVerdict } from '../lib/verdict-nav'

  let { id }: { id: string } = $props()

  let details = $state<ApprovalDetails | null>(null)
  let fields = $state<Field[]>([])
  let values = $state<Record<string, FieldValue>>({})
  /** Prose fields showing their source instead of the rendered document. */
  let raw = $state<Record<string, boolean>>({})
  /** The reason with Reject, the notes with Request changes. */
  let said = $state('')
  /** The node's 422, by the field it names. */
  let refused = $state<Record<string, string[]>>({})
  let busy = $state(false)
  let error = $state<string | null>(null)
  let loaded = $state(false)
  /** The node said there is no such approval, as opposed to failing to say. */
  let missing = $state(false)
  let decided = $state(false)
  /** The route this approval was opened from; a verdict goes back to it. */
  let openedFrom: string | null = null

  function load() {
    return api
      .approval(id)
      .then((d) => {
        const args = asArguments(d.arguments)
        details = d
        fields = approvalFields(schemaFor(d.input_schema, args), args, d.prose_fields, d.locked_fields)
        values = formValues(fields, args)
        loaded = true
      })
      .catch((e) => {
        error = e instanceof Error ? e.message : String(e)
        missing = isNotFound(e)
        loaded = true
      })
  }

  function retry() {
    loaded = false
    error = null
    void load()
  }

  $effect(() => {
    void id
    loaded = false
    details = null
    error = null
    missing = false
    refused = {}
    said = ''
    decided = false
    openedFrom = router.previous
    void load()
    // Release on navigating away. The node's sweeper covers a client that
    // vanishes without getting here.
    return () => {
      if (!decided) void api.releaseApproval(id).catch(() => {})
    }
  })

  const original = $derived(asArguments(details?.arguments))
  const schema = $derived(schemaFor(details?.input_schema ?? null, original))
  const pending = $derived(details?.approval.state === 'pending')
  const lapsed = $derived(pending && details !== null && details.approval.expires_ms <= clock.now)
  const open = $derived(pending && !lapsed)
  const edited = $derived(approvalArguments(fields, values, original))
  const changes = $derived(diffArguments(original, edited))
  const problems = $derived(
    problemsByField(
      changes.length ? editProblems(schema, original, edited) : [],
      fields.map((f) => f.key),
    ),
  )
  const blocked = $derived(Object.keys(problems).length > 0)
  /** What the document shows: the operator's draft while it waits, what ran once it is decided. */
  const shown = $derived(
    pending ? edited : details?.edited_arguments ? asArguments(details.edited_arguments) : original,
  )
  const prose = $derived(
    (details?.prose_fields ?? []).filter((k) => typeof shown[k] === 'string' && shown[k] !== ''),
  )
  const settledChanges = $derived(
    !pending && details?.edited_arguments ? diffArguments(original, asArguments(details.edited_arguments)) : [],
  )
  /** Arguments the tool's schema does not name: sent as they are, shown so nothing runs unseen. */
  const unnamed = $derived(Object.keys(original).filter((k) => !fields.some((f) => f.key === k)))
  const can = $derived(new Set((details?.options ?? []).map((o) => o.kind)))
  /** Allowing opens a grant rather than running the call, so there is nothing to edit. */
  const grant = $derived(grantsAccess(details?.options ?? []))
  /** What the operator reads in a grant: what was asked, without the agent's own wait. */
  const asked = $derived(
    Object.keys(original).filter((k) => !CALLER_ONLY.includes(k) && original[k] !== null && original[k] !== ''),
  )
  const result = $derived(details?.result === null || details?.result === undefined ? null : details.result)
  const rows = $derived(resultRows(result))
  const fallback = $derived(details?.approval.session_id ? `/sessions/${details.approval.session_id}` : '/')

  function html(text: string): string {
    return details?.format === 'jira_wiki' ? renderJiraWiki(text) : render(text)
  }

  function display(v: unknown): string {
    if (v === undefined || v === null || v === '') return '—'
    return typeof v === 'string' ? v : JSON.stringify(v)
  }

  function set(key: string, value: FieldValue) {
    values[key] = value
    if (refused[key]) {
      const { [key]: _, ...rest } = refused
      refused = rest
    }
  }

  async function answer(optionId: 'allow_once' | 'allow_session' | 'allow_repo' | 'reject_once' | 'request_changes') {
    if (!details || !open) return
    busy = true
    error = null
    refused = {}
    const text = said.trim()
    try {
      decided = true
      await api.answer(
        id,
        optionId,
        optionId === 'allow_once' ? changedArguments(original, edited) : undefined,
        optionId === 'reject_once' ? { reason: text } : optionId === 'request_changes' ? { notes: text } : {},
      )
      await store.refetch()
      leaveAfterVerdict(router, { prefix: '/approvals', id, from: openedFrom, fallback })
    } catch (e) {
      decided = false
      if (e instanceof ApiError && e.status === 422 && e.fields?.length) {
        refused = problemsByField(e.fields, [...fields.map((f) => f.key), 'reason', 'notes'])
        error = 'the node refused this answer as sent; nothing was decided'
      } else if (e instanceof ApiError && e.status === 409) {
        error = e.message
        await load()
      } else {
        error = e instanceof Error ? e.message : String(e)
      }
    } finally {
      busy = false
    }
  }
</script>

{#if !loaded}
  <div class="empty">Loading approval…</div>
{:else if !details && missing}
  <div class="banner crit">not found <b>· {error ?? 'no such approval'}</b></div>
{:else if !details}
  <div class="banner crit" role="alert">
    Could not load this approval <b>· {error}</b>
    <button class="lnk" onclick={retry}>Retry</button>
  </div>
{:else}
  {@const a = details.approval}
  <div class="head" class:done={!open}>
    <span class="bar"></span>
    <span class="mono">{formatAge(a.created_ms, clock.now)}</span>
    <span class="t">
      <em>Approval</em>
      {a.title}
      <small
        >{a.tool} · {a.channel} · {pending
          ? lapsed
            ? 'expired · nothing ran'
            : `${formatExpiry(a.expires_ms, clock.now)} · ${grant ? 'nothing is open yet; allowing it grants the request' : 'nothing is waiting; allowing it runs the call'}`
          : a.state.replace('_', ' ')}</small
      >
    </span>
  </div>

  <dl class="kv">
    {#if a.session_id}
      <dt>Session</dt>
      <dd class="m"><a href="/sessions/{a.session_id}">{a.session_id.slice(0, 8)}</a></dd>
    {:else}
      <dt>Asked by</dt>
      <dd class="m">{details.lane ?? 'an external agent'}</dd>
    {/if}
    {#if a.decided_ms !== null}
      <dt>Decided</dt>
      <dd class="m">{formatAge(a.decided_ms, clock.now)} ago</dd>
    {/if}
    {#if a.finished_ms !== null}
      <dt>{a.state === 'expired' ? 'Expired' : 'Finished'}</dt>
      <dd class="m">{formatAge(a.finished_ms, clock.now)} ago</dd>
    {/if}
  </dl>

  {#if pending && details.claimed_before_ms !== null}
    <div class="banner dim">
      already open elsewhere <b>· another tab or device opened this {formatAge(details.claimed_before_ms, clock.now)} ago; whichever answers first decides it</b>
    </div>
  {/if}

  {#if !pending}
    <div class="banner" class:ok={a.state === 'succeeded' || a.state === 'running'} class:crit={a.state === 'failed' || a.state === 'uncertain'} class:dim={a.state === 'rejected' || a.state === 'expired' || a.state === 'changes_requested'}>
      {outcome(a.state)}
      {#if a.state === 'changes_requested' && a.operator_note}
        <i>Notes: {a.operator_note}</i>
      {:else if a.reason}
        <i>Reason: {a.reason}</i>
      {/if}
      {#if a.state !== 'changes_requested' && a.operator_note}
        <i>Note: {a.operator_note}</i>
      {/if}
    </div>
  {:else if lapsed}
    <div class="banner dim">expired unanswered <b>· nothing ran; the agent can ask again</b></div>
  {/if}

  {#if !pending && result !== null}
    <div class="h4">Result <b>what the call returned</b></div>
    {#if rows}
      <dl class="kv result">
        {#each rows as row (row.key)}
          <dt>{row.key}</dt>
          <dd class="m">
            {#if row.href}<a href={row.href} target="_blank" rel="noopener noreferrer">{row.value}</a>{:else}{row.value}{/if}
          </dd>
        {/each}
      </dl>
    {:else}
      <pre class="src">{typeof result === 'string' ? result : JSON.stringify(result, null, 2)}</pre>
    {/if}
  {/if}

  {#each prose as key (key)}
    <section class="doc">
      <div class="dochead">
        <span>{key}</span>
        <button class="lnk" type="button" onclick={() => (raw[key] = !raw[key])}>
          {raw[key] ? 'Rendered' : 'Raw source'}
        </button>
      </div>
      {#if raw[key]}
        <pre class="src">{shown[key]}</pre>
      {:else}
        <article class="md">{@html html(String(shown[key]))}</article>
      {/if}
    </section>
  {/each}

  {#if open && grant}
    <div class="h4">Request <b>as the agent asked it</b></div>
    <dl class="kv">
      {#each asked as key (key)}
        <dt>{key}</dt>
        <dd class:m={typeof original[key] !== 'string' || !String(original[key]).includes(' ')}>{display(original[key])}</dd>
      {/each}
    </dl>
  {:else if open}
    <div class="h4">Arguments <b>edit before allowing if you want to</b></div>
    <div class="form">
      {#each fields as f (f.key)}
        {@const v = values[f.key]}
        {@const msgs = [...(problems[f.key] ?? []), ...(refused[f.key] ?? [])]}
        <label class="field" class:bad={msgs.length > 0}>
          <span class="name"
            >{f.key}{f.required ? '' : ' · optional'}{#if f.locked}<span
                class="chip"
                title="names what this call acts on; reject it and ask for a new call to change it">locked</span
              >{/if}</span
          >
          {#if f.locked}
            <code class="locked">{display(original[f.key])}</code>
          {:else if f.kind === 'prose' || f.kind === 'json'}
            <textarea
              class:mono={f.kind === 'json'}
              value={String(v ?? '')}
              use:autogrow={String(v ?? '')}
              disabled={busy}
              spellcheck={f.kind === 'prose'}
              oninput={(e) => set(f.key, e.currentTarget.value)}
            ></textarea>
          {:else if f.kind === 'tags'}
            <!-- One entry per line: a comma can sit inside a command. -->
            <textarea
              class="list mono"
              value={Array.isArray(v) ? v.join('\n') : String(v ?? '')}
              use:autogrow={Array.isArray(v) ? v.join('\n') : String(v ?? '')}
              placeholder="one per line"
              disabled={busy}
              spellcheck="false"
              oninput={(e) => set(f.key, e.currentTarget.value.split('\n'))}
            ></textarea>
          {:else if f.kind === 'boolean'}
            <input
              type="checkbox"
              checked={v === true}
              disabled={busy}
              onchange={(e) => set(f.key, e.currentTarget.checked)}
            />
          {:else if f.kind === 'select'}
            <select value={String(v ?? '')} disabled={busy} onchange={(e) => set(f.key, e.currentTarget.value)}>
              {#if !f.required}<option value="">—</option>{/if}
              {#each f.options ?? [] as o (o)}<option value={o}>{o}</option>{/each}
            </select>
          {:else}
            <input
              value={String(v ?? '')}
              inputmode={f.kind === 'integer' || f.kind === 'number' ? 'decimal' : undefined}
              disabled={busy}
              oninput={(e) => set(f.key, e.currentTarget.value)}
            />
          {/if}
          {#if f.description}<small>{f.description}</small>{/if}
          {#each msgs as m, i (i)}<small class="err">{m}</small>{/each}
        </label>
      {/each}
      {#each unnamed as key (key)}
        <div class="field">
          <span class="name"
            >{key}<span class="chip" title="the tool's schema does not name it, so it is sent as the agent wrote it"
              >not in the schema</span
            ></span
          >
          <code class="locked">{display(original[key])}</code>
        </div>
      {/each}
    </div>
  {/if}

  {#if open ? changes.length : settledChanges.length}
    <div class="h4">{open ? 'What you changed' : 'What the operator changed'} <b>{open ? 'Allow runs it as edited' : 'it ran as edited'}</b></div>
    <div class="changes">
      {#each open ? changes : settledChanges as c (c.key)}
        <div class="change">
          <div class="ck">{c.key} · {c.kind}</div>
          <div class="lines">
            {#each c.lines as l, i (i)}
              <div class={l.kind}>{l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ' '} {l.text}</div>
            {/each}
          </div>
        </div>
      {/each}
    </div>
  {/if}

  {#if !open}
    <details class="request">
      <summary>Arguments as the agent sent them</summary>
      <pre>{JSON.stringify(original, null, 2)}</pre>
    </details>
  {/if}

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
  {/if}
  {#each refused[''] ?? [] as m, i (i)}
    <div class="banner crit">{m}</div>
  {/each}

  {#if open}
    <div class="decide">
      {#if can.has('allow_once')}
        <button class="btn p" disabled={busy || blocked} onclick={() => answer('allow_once')}>
          {can.has('allow_repo') ? 'Allow once' : changes.length ? 'Allow with edits' : 'Allow'}
        </button>
      {/if}
      <!-- A host a session asked to reach: how long it stays open. -->
      {#if can.has('allow_session')}
        <button class="btn" disabled={busy} onclick={() => answer('allow_session')}>For this session</button>
      {/if}
      {#if can.has('allow_repo')}
        <button class="btn" disabled={busy} onclick={() => answer('allow_repo')}>Save to the repository's egress</button>
      {/if}
      <input
        bind:value={said}
        class:bad={(refused.notes ?? refused.reason ?? []).length > 0}
        oninput={() => {
          const { notes: _n, reason: _r, ...rest } = refused
          refused = rest
        }}
        placeholder="What to change, or why you are rejecting · goes back to the agent"
        disabled={busy}
      />
      {#if can.has('request_changes')}
        <button class="btn" disabled={busy || !said.trim()} onclick={() => answer('request_changes')}>
          Request changes
        </button>
      {/if}
      {#if can.has('reject_once')}
        <button class="btn d" disabled={busy || !said.trim()} onclick={() => answer('reject_once')}>Reject</button>
      {/if}
      {#each [...(refused.notes ?? []), ...(refused.reason ?? [])] as m, i (i)}
        <small class="err">{m}</small>
      {/each}
    </div>
  {/if}
{/if}

<style>
  .head {
    display: grid;
    grid-template-columns: 3px 72px minmax(0, 1fr);
    gap: 0 14px;
    align-items: center;
    background: linear-gradient(90deg, var(--wash-wait), var(--s1) 42%);
    border-radius: 4px;
    padding: 10px 14px 10px 0;
    overflow: hidden;
  }
  .head .bar {
    align-self: stretch;
    background: var(--wait);
    border-radius: 2px 0 0 2px;
  }
  .head.done {
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 42%);
  }
  .head.done .bar {
    background: var(--dim);
  }
  .t {
    font-weight: 500;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--wait);
  }
  .head.done .t em {
    color: var(--dim);
  }
  .t small {
    display: block;
    font: 12px var(--mono);
    color: var(--dim);
    margin-top: 2px;
  }
  .kv {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 5px 16px;
    font-size: 13.5px;
    margin: 0;
  }
  .kv dt {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    padding-top: 3px;
  }
  .kv dd {
    margin: 0;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .kv dd.m {
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .doc {
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px 4px;
    min-width: 0;
  }
  .dochead {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 10px;
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
    margin-bottom: 6px;
  }
  .dochead .lnk {
    text-transform: none;
    letter-spacing: 0;
    font-size: 12px;
  }
  .src,
  .request pre {
    margin: 0 0 10px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: 12px/1.5 var(--mono);
    color: var(--ink);
  }
  .md {
    max-width: 72ch;
    line-height: 1.55;
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  .md :global(h1),
  .md :global(h2),
  .md :global(h3),
  .md :global(h4),
  .md :global(h5),
  .md :global(h6) {
    font-weight: 600;
    line-height: 1.25;
    margin: 1.2em 0 0.5em;
  }
  .md :global(h1) {
    font-size: 20px;
    margin-top: 0.3em;
  }
  .md :global(h2) {
    font-size: 17px;
  }
  .md :global(h3) {
    font-size: 14.5px;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol),
  .md :global(blockquote),
  .md :global(pre),
  .md :global(table) {
    margin: 0 0 0.9em;
  }
  .md :global(code) {
    font: 12.5px var(--mono);
    background: var(--s2);
    border-radius: 3px;
    padding: 1px 4px;
  }
  .md :global(pre) {
    background: var(--s2);
    border-radius: 4px;
    padding: 10px 12px;
    overflow-x: auto;
  }
  .md :global(pre code) {
    background: none;
    padding: 0;
  }
  .md :global(blockquote) {
    border-left: 3px solid var(--s3);
    padding-left: 12px;
    color: var(--ink2);
  }
  .md :global(table) {
    border-collapse: collapse;
    font-size: 13px;
    display: block;
    overflow-x: auto;
  }
  .md :global(th),
  .md :global(td) {
    border-bottom: 1px solid var(--s2);
    padding: 5px 10px 5px 0;
    text-align: left;
    vertical-align: top;
  }
  .md :global(.mention) {
    color: var(--acc);
  }
  .form {
    display: grid;
    gap: 12px;
    min-width: 0;
  }
  .field {
    display: grid;
    gap: 4px;
    min-width: 0;
  }
  .field .name {
    font: 11.5px var(--mono);
    color: var(--ink2);
    display: flex;
    gap: 8px;
    align-items: baseline;
  }
  .field small {
    font-size: 12px;
    color: var(--dim);
  }
  .field input:not([type='checkbox']),
  .field select,
  .field textarea,
  .decide input {
    width: 100%;
    box-sizing: border-box;
    background: var(--s1);
    border: 1px solid transparent;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13.5px var(--sans);
  }
  .field input[type='checkbox'] {
    justify-self: start;
  }
  .field textarea {
    min-height: 110px;
    max-height: 70vh;
    resize: vertical;
    line-height: 1.5;
  }
  .field textarea.mono {
    font: 12.5px/1.45 var(--mono);
  }
  .field textarea.list {
    min-height: 0;
    white-space: pre;
    overflow-x: auto;
  }
  .field.bad input,
  .field.bad textarea,
  .field.bad select,
  .decide input.bad {
    border-color: var(--crit);
  }
  small.err {
    color: var(--crit);
    font: 12px var(--mono);
  }
  .locked {
    font: 12.5px var(--mono);
    color: var(--ink2);
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 10px;
    overflow-wrap: anywhere;
  }
  .changes {
    display: grid;
    gap: 8px;
  }
  .change {
    background: var(--s2);
    border-radius: 4px;
    overflow: hidden;
  }
  .ck {
    font: 11.5px var(--mono);
    color: var(--dim);
    padding: 6px 12px 2px;
  }
  .lines {
    font: 12px/1.55 var(--mono);
    padding: 2px 0 8px;
  }
  .lines div {
    padding: 0 12px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .lines .add {
    background: color-mix(in srgb, var(--ok) 16%, transparent);
  }
  .lines .del {
    background: color-mix(in srgb, var(--crit) 16%, transparent);
  }
  .request {
    color: var(--dim);
  }
  .request summary {
    cursor: pointer;
    font: 11.5px var(--mono);
  }
  .request pre {
    background: var(--s1);
    border-radius: 4px;
    padding: 8px 10px;
    margin-top: 6px;
    max-height: 360px;
    overflow: auto;
  }
  .decide {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    align-items: center;
    border-top: 1px solid var(--rule);
    padding-top: 14px;
  }
  .decide input {
    flex: 1;
    min-width: 200px;
    width: auto;
    font-size: 13px;
  }
  .decide small.err {
    flex-basis: 100%;
  }
  @media (max-width: 700px) {
    .head {
      grid-template-columns: 3px auto minmax(0, 1fr);
      gap: 0 10px;
      padding-right: 10px;
    }
    .doc {
      padding: 10px 10px 4px;
    }
    .md :global(table) {
      font-size: 12px;
    }
    .md :global(th),
    .md :global(td) {
      padding-right: 8px;
      white-space: nowrap;
    }
  }
</style>
