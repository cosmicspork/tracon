<script lang="ts">
  import { api, ApiError } from '../lib/api'
  import Name from '../components/Name.svelte'
  import PluginCardContent from '../components/PluginCardContent.svelte'
  import { askedBy, pluginCard } from '../lib/plugin-card'
  import {
    allowGate,
    approvalArguments,
    approvalFields,
    asArguments,
    CALLER_ONLY,
    changedArguments,
    editProblems,
    grantsAccess,
    itemSections,
    outcome,
    pluginRows,
    previewHash,
    previewOutdated,
    problemsByField,
    amendsNothing,
    renderedProse,
    replacement,
    resultRows,
    schemaFor,
    type ApprovalDetails,
  } from '../lib/approval'
  import { autogrow } from '../lib/autogrow'
  import { clock } from '../lib/clock.svelte'
  import { formatAge, formatExpiry } from '../lib/format'
  import { isNotFound } from '../lib/load'
  import { codeSpans, render } from '../lib/markdown'
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
  /** The arguments the plugin's preview was last taken of. */
  let previewedFor = $state.raw<Record<string, unknown> | null>(null)
  /** Why the edit's preview could not be asked for. */
  let previewError = $state<string | null>(null)
  /** The arguments the plugin card's "Lent on allow" was last computed for. */
  let lentFor = $state.raw<Record<string, unknown> | null>(null)
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
        previewedFor = args
        lentFor = args
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
    drafts = {}
    previewedFor = null
    previewError = null
    lentFor = null
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
  /** A card a plugin raised: its content is shown as it is, and takes no edit. */
  const card = $derived(details ? pluginCard(details.approval.tool, original) : null)
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
  /** The preview shows the call as asked, not as edited: Allow waits for the edit's. */
  const outdated = $derived(pending && previewOutdated(details, previewedFor, edited))
  /**
   * "Lent on allow" is of the call as asked, not as edited: the node says
   * what the edit lends, by the dispatch's own function, and Allow waits.
   */
  const lentOutdated = $derived(
    pending && !!details?.plugin && lentFor !== null && diffArguments(lentFor, edited).length > 0,
  )
  /** Allow, held until the call as it would run has been previewed. */
  const allow = $derived(allowGate(details, { busy, blocked, outdated: outdated || lentOutdated }))
  /** What the document shows: the operator's draft while it waits, what ran once it is decided. */
  const shown = $derived(
    pending ? edited : details?.edited_arguments ? asArguments(details.edited_arguments) : original,
  )
  /**
   * While it waits, the draft against what it would replace: the document
   * as it stands, or nothing for a new one. Once decided the document is
   * what ran, so there is nothing left to compare.
   */
  const replacing = $derived(pending && details ? replacement(details, edited) : null)
  const prose = $derived(
    (details?.prose_fields ?? []).filter(
      (k) => typeof shown[k] === 'string' && shown[k] !== '' && k !== replacing?.key,
    ),
  )
  /** Each item of a call that carries a list of them, as it would run; the list is edited below as JSON. */
  const sections = $derived(itemSections(schema, shown, details?.item_list, details?.prose_fields ?? []))
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

  /** A page for the preview origin's sandbox: shown as text here, never rendered. */
  const source = $derived(details?.format === 'source')

  /**
   * The operator's drafts of prose in a plugin's format, as its owner
   * rendered them, by the text rendered. A draft not rendered yet, or that
   * the owner could not render, is shown as source.
   */
  let drafts = $state<Record<string, string>>({})

  /**
   * A prose field as HTML, or null to show its source. Prose in a plugin's
   * format is shown as that plugin rendered it, and as source when it could
   * not be.
   */
  function html(key: string, text: string, item?: number): string | null {
    if (!details) return null
    const md = renderedProse(details, key, text, item) ?? drafts[text]
    if (md !== undefined) return render(md)
    return null
  }

  /** The prose texts of the draft no rendering covers yet. */
  const unrendered = $derived.by(() => {
    if (!details?.plugin_format || !pending) return []
    const texts: { key: string; text: string; item?: number }[] = [
      ...prose.map((k) => ({ key: k, text: String(shown[k]) })),
      ...sections.flatMap((item) => item.prose.map((p) => ({ key: p.key, text: p.text, item: item.index }))),
    ]
    return texts
      .filter((t) => renderedProse(details!, t.key, t.text, t.item) === null && drafts[t.text] === undefined)
      .map((t) => t.text)
  })

  // Render the operator's edits through the owning plugin, once they pause.
  $effect(() => {
    const texts = unrendered
    if (texts.length === 0 || details?.render_error) return
    const timer = setTimeout(() => {
      for (const text of texts) {
        api
          .renderApproval(id, text)
          .then((r) => (drafts[text] = r.markdown))
          .catch(() => {})
      }
    }, 300)
    return () => clearTimeout(timer)
  })

  // Preview the operator's edit through the plugin once they pause, so
  // what Allow is bound to is the edit's effect, not the call as asked.
  $effect(() => {
    if ((!outdated && !lentOutdated) || blocked) return
    const args = edited
    // An answer that arrives after a later edit is not that edit's preview.
    const current = () => diffArguments(args, edited).length === 0
    const timer = setTimeout(() => {
      api
        .previewApproval(id, args)
        .then((r) => {
          if (!details || !current()) return
          previewError = null
          if (r.preview) details.preview = r.preview
          details.allow_blocked = r.allow_blocked ?? null
          if (r.plugin) details.plugin = r.plugin
          previewedFor = args
          lentFor = args
        })
        .catch((e) => {
          if (current()) previewError = e instanceof Error ? e.message : String(e)
        })
    }, 400)
    return () => clearTimeout(timer)
  })

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
        {
          ...(optionId === 'reject_once' ? { reason: text } : optionId === 'request_changes' ? { notes: text } : {}),
          argumentsHash: details.arguments_hash,
          previewHash: previewHash(details),
        },
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
        ><Name kind="approval" id={a.id} renamable /> · {a.tool} · {a.channel} · {pending
          ? lapsed
            ? 'expired · nothing ran'
            : `${formatExpiry(a.expires_ms, clock.now)} · ${grant ? 'allowing grants it' : 'allowing runs it'}`
          : a.state.replace('_', ' ')}</small
      >
    </span>
  </div>

  <dl class="kv">
    {#if card}
      <dt>Asked by</dt>
      <dd class="m">{askedBy(card)}</dd>
    {/if}
    {#if a.session_id}
      <dt>Session</dt>
      <dd class="m"><Name kind="session" id={a.session_id} href="/sessions/{a.session_id}" /></dd>
    {:else if !card}
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
    {#each pluginRows(details, lentOutdated) as row (row.key)}
      <dt>{row.key}</dt>
      <dd class="m">{row.value}</dd>
    {/each}
  </dl>

  {#if pending && details.claimed_before_ms !== null}
    <div class="banner dim">
      already open elsewhere <b>· another tab or device opened this {formatAge(details.claimed_before_ms, clock.now)} ago; whichever answers first decides it</b>
    </div>
  {/if}

  {#if !pending}
    <div class="banner" class:ok={a.state === 'succeeded' || a.state === 'running'} class:crit={a.state === 'failed' || a.state === 'uncertain'} class:dim={a.state === 'rejected' || a.state === 'expired' || a.state === 'changes_requested'}>
      {outcome(a.state, a.reason)}
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

  {#if pending && amendsNothing(details)}
    <div class="banner crit" role="alert">
      the memory is gone or no longer live <b>· there is nothing left to amend; Allow will fail, so reject</b>
    </div>
  {/if}

  {#if (outdated || lentOutdated) && !blocked}
    <div class="banner dim" role="status">
      {#if previewError}
        your edit could not be previewed <b>· {previewError} · Allow waits for a preview of it</b>
      {:else if outdated}
        previewing your edit <b>· Allow waits for it</b>
      {:else}
        reading what your edit lends <b>· Allow waits for it</b>
      {/if}
    </div>
  {/if}

  {#if pending && details.preview?.status === 'refused'}
    <div class="banner crit" role="alert">
      the plugin refused the preview <b>· {details.preview.error} · {allow.why ? 'you can still reject it or ask for changes' : 'you can still decide'}</b>
    </div>
  {:else if pending && details.preview?.status === 'shown'}
    {@const p = details.preview}
    <div class="h4">
      Preview <b>{p.summary}{p.previewable ? '' : ' · not previewable'}{outdated ? ' · of the call before your edit' : ''}</b>
    </div>
    <section class="doc preview">
      <article class="md">{@html render(p.body)}</article>
    </section>
  {/if}

  {#if replacing}
    {#if details.stale && details.proposed_body === null}
      <div class="banner crit" role="alert">
        the edits no longer apply to the document as it is now <b
          >· Allow will fail, so ask for changes or reject</b
        >
      </div>
    {:else if details.stale}
      <div class="banner crit" role="alert">
        the document changed since the agent read it <b
          >· this diff is against the document as it is now; Allow will fail as a conflict, so ask for changes
          or reject</b
        >
      </div>
    {/if}
    <div class="h4">
      Changes to the {replacing.subject} <b
        >{replacing.label}{replacing.format &&
        replacing.format !== 'markdown'
          ? ` · ${replacing.format}`
          : ''} · +{replacing.added} −{replacing.removed}</b
      >
    </div>
    <div class="change">
      <div class="lines">
        {#each replacing.hunks as h, i (i)}
          {#if h.kind === 'skip'}
            <div class="skip">… {h.count} unchanged {h.count === 1 ? 'line' : 'lines'}</div>
          {:else}
            {#each h.lines as l, j (j)}
              <div class={l.kind}>{l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ' '} {l.text}</div>
            {/each}
          {/if}
        {/each}
      </div>
    </div>
  {/if}

  {#each prose as key (key)}
    {@const rendered = raw[key] || source ? null : html(key, String(shown[key]))}
    <section class="doc">
      <div class="dochead">
        <span>{key}</span>
        {#if source}
          <span>source · runs only in the preview</span>
        {:else}
          <button class="lnk" type="button" onclick={() => (raw[key] = !raw[key])}>
            {raw[key] ? 'Rendered' : 'Raw source'}
          </button>
        {/if}
      </div>
      {#if rendered === null}
        <pre class="src">{shown[key]}</pre>
      {:else}
        <article class="md">{@html rendered}</article>
      {/if}
    </section>
  {/each}

  {#if card}
    <PluginCardContent blocks={card.content} />
  {/if}

  {#each sections as item (item.index)}
    {@const list = details.item_list ?? ''}
    <section class="doc item">
      <div class="dochead">
        <span>{list}[{item.index}] · #{item.index}</span>
      </div>
      {#if item.rows.length}
        <dl class="kv">
          {#each item.rows as row (row.key)}
            <dt>{row.key}</dt>
            <dd class:m={!row.value.includes(' ')}>{row.value}</dd>
          {/each}
        </dl>
      {/if}
      {#each item.prose as p (p.key)}
        {@const at = `${list}[${item.index}].${p.key}`}
        <div class="dochead sub">
          <span>{p.key}</span>
          {#if source}
            <span>source · runs only in the preview</span>
          {:else}
            <button class="lnk" type="button" onclick={() => (raw[at] = !raw[at])}>
              {raw[at] ? 'Rendered' : 'Raw source'}
            </button>
          {/if}
        </div>
        {@const rendered = raw[at] || source ? null : html(p.key, p.text, item.index)}
        {#if rendered === null}
          <pre class="src">{p.text}</pre>
        {:else}
          <article class="md">{@html rendered}</article>
        {/if}
      {/each}
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
  {:else if open && !card}
    <div class="h4">Arguments <b>edit before allowing if you want to</b></div>
    <div class="form">
      {#each fields as f (f.key)}
        {@const v = values[f.key]}
        {@const msgs = [...(problems[f.key] ?? []), ...(refused[f.key] ?? [])]}
        {#if f.key === replacing?.key && !f.locked}
          <!-- The diff above is how it reads; the text is here to edit, and the diff follows it. -->
          <div class="field" class:bad={msgs.length > 0}>
            <details class="fulltext">
              <summary class="name">Full text · {f.key}</summary>
              <textarea
                class="grow"
                aria-label={f.key}
                value={String(v ?? '')}
                use:autogrow={String(v ?? '')}
                disabled={busy}
                spellcheck="true"
                oninput={(e) => set(f.key, e.currentTarget.value)}
              ></textarea>
            </details>
            {#each msgs as m, i (i)}<small class="err">{m}</small>{/each}
          </div>
        {:else}
          <label class="field" class:bad={msgs.length > 0}>
            <span class="name"
              >{f.key}{f.required ? '' : ' · optional'}{#if f.locked}<span
                  class="chip"
                  title="Fixed by the call. Reject to change it.">locked</span
                >{/if}</span
            >
            {#if f.locked}
              <code class="locked">{display(original[f.key])}</code>
            {:else if f.kind === 'prose' || f.kind === 'json'}
              <textarea
                class="grow"
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
                class="list mono grow"
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
            {#if f.description}<small>{@html codeSpans(f.description)}</small>{/if}
            {#each msgs as m, i (i)}<small class="err">{m}</small>{/each}
          </label>
        {/if}
      {/each}
      {#each unnamed as key (key)}
        <div class="field">
          <span class="name"
            >{key}<span class="chip" title="Not in the tool's schema; sent as written"
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

  {#if open && allow.why}
    <div class="banner crit" role="status" data-testid="allow-held">
      Allow waits for a preview <b>· {@html codeSpans(allow.why)}</b>
    </div>
  {/if}

  {#if open}
    <div class="decide">
      {#if can.has('allow_once')}
        <button class="btn p" disabled={allow.disabled} title={allow.why ?? undefined} onclick={() => answer('allow_once')}>
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
  .item .kv {
    margin-bottom: 10px;
  }
  .dochead.sub {
    margin-top: 4px;
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
  /*
   * A cell keeps its words whole and a header stays on one line: a table
   * wider than its column scrolls rather than squeezing each column to a
   * letter, as `overflow-wrap: anywhere` above would let it.
   */
  .md :global(th),
  .md :global(td) {
    border-bottom: 1px solid var(--s2);
    padding: 5px 10px 5px 0;
    text-align: left;
    vertical-align: top;
    overflow-wrap: normal;
  }
  .md :global(th) {
    white-space: nowrap;
  }
  /*
   * A plugin's preview is mostly data (the rows a call would change), so it
   * takes the page's width and its cells stay on one line; its prose keeps
   * the reading measure.
   */
  .preview .md {
    max-width: none;
  }
  .preview .md :global(td) {
    white-space: nowrap;
  }
  .preview .md > :global(:not(table, pre)) {
    max-width: 72ch;
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
    line-height: 1.5;
  }
  .field textarea.mono {
    font: 12.5px/1.45 var(--mono);
  }
  .field textarea.list {
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
  .lines .skip {
    color: var(--dim);
    font-style: italic;
  }
  .fulltext summary {
    cursor: pointer;
  }
  .fulltext[open] summary {
    margin-bottom: 4px;
  }
  .fulltext textarea {
    display: block;
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
