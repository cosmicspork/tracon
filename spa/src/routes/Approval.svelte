<script lang="ts">
  import ReportReview from '../components/ReportReview.svelte'
  import Diff from '../components/Diff.svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import { autogrow } from '../lib/autogrow'
  import { surface } from '../lib/surface.svelte'
  import {
    reviewChecks,
    reviewVerdict,
    type CandidateEvidence,
    type Criteria,
    type PinnedRequirements,
    type Review,
    type ReviewContext,
    type ReviewIntent,
    type ReviewOutputs,
    type ReviewRevisionRef,
    type Verdict,
  } from '../lib/types'
  import { baseFromDiff, buildPatch, fileSection } from '../lib/patch'
  import { isNarrativeReport } from '../lib/reports'
  import { leaveAfterVerdict } from '../lib/verdict-nav'
  import { COVERAGE, VERDICTS, attention, linkSays, whatIsLeft } from '../lib/criteria'

  let { id }: { id: string } = $props()

  let review = $state<Review | null>(null)
  /** The revision this screen is showing; the verdict names it. */
  let revision = $state<ReviewRevisionRef | null>(null)
  let stale = $state<string[]>([])
  let evidence = $state<CandidateEvidence | null>(null)
  let requirements = $state<PinnedRequirements | null>(null)
  /** The pinned item's criteria against this revision's candidate, if any. */
  let criteria = $state<Criteria | null>(null)
  /** The criterion whose verdict is being given, by key. */
  let judging = $state<string | null>(null)
  let criterionVerdict = $state<Verdict>('met')
  let criterionNote = $state('')
  let criteriaError = $state<string | null>(null)
  let surroundingCode = $state<ReviewContext[]>([])
  let reason = $state('')
  let title = $state('')
  let body = $state('')
  /** What approval sends to the forge, as the agent asked and you edit it. */
  let intent = $state<ReviewIntent>({ forge: {} })
  let describe = $state(false)
  let descTitle = $state('')
  let descBody = $state('')
  let commenting = $state(false)
  let comment = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)
  let loaded = $state(false)
  /** The route this review was opened from; a verdict goes back to it. */
  let openedFrom: string | null = null
  const report = $derived(review && isNarrativeReport(review) ? review : null)

  $effect(() => {
    void id
    loaded = false
    openedFrom = router.previous
    api
      .review(id)
      .then((d) => {
        review = d.review
        revision = d.revision
        stale = d.stale
        evidence = d.evidence
        requirements = d.requirements
        criteria = d.criteria
        surroundingCode = d.surrounding_code
        title = d.review.edited_title ?? d.review.title
        body = d.review.edited_body ?? d.review.body
        intent = d.intent ?? { forge: {} }
        describe = intent.forge.description !== undefined
        descTitle = intent.forge.description?.title ?? ''
        descBody = intent.forge.description?.body ?? ''
        commenting = intent.forge.comment !== undefined
        comment = intent.forge.comment ?? ''
        loaded = true
      })
      .catch((e) => {
        error = e instanceof Error ? e.message : String(e)
        loaded = true
      })
    // Release on navigating away. The node's sweeper covers a client that
    // vanishes without getting here.
    return () => {
      if (!decided) void api.releaseReview(id).catch(() => {})
    }
  })

  let decided = $state(false)

  // Editing the diff, desktop only. The phone directs; it does not edit.
  let editing = $state(false)
  // CodeMirror is most of a megabyte and the phone never opens it, so it is
  // fetched when the operator asks to edit rather than on every page load.
  let DiffEditor = $state<typeof import('../components/DiffEditor.svelte').default | null>(null)
  let loadingFiles = $state(false)
  let fileError = $state<string | null>(null)
  /** path → { original: before the change, head: as submitted, now: edited } */
  let editable = $state<Map<string, { original: string; head: string; now: string }>>(new Map())

  // Keyed on the revision, not the commit: a resubmission may carry the same
  // commit, and an edit written against the revision it replaced is not an
  // edit of this one.
  const draftKey = $derived(
    review ? `tracon-edit-${review.id}-${revision?.id ?? review.head_sha}` : '',
  )
  const patch = $derived.by(() =>
    buildPatch(
      [...editable.entries()]
        .filter(([, f]) => f.now !== f.head)
        .map(([path, f]) => ({ path, before: f.head, after: f.now })),
    ),
  )
  const editedFiles = $derived([...editable.entries()].filter(([, f]) => f.now !== f.head).length)
  const publishing = $derived(review?.state === 'publishing')

  /** Fetch each reviewed file as submitted and rebuild what it changed from. */
  async function startEditing() {
    if (!review) return
    editing = true
    loadingFiles = true
    fileError = null
    try {
      DiffEditor ??= (await import('../components/DiffEditor.svelte')).default
    } catch (e) {
      fileError = e instanceof Error ? e.message : String(e)
      loadingFiles = false
      return
    }
    if (editable.size > 0) {
      loadingFiles = false
      return
    }
    const next = new Map<string, { original: string; head: string; now: string }>()
    try {
      let drafts: Record<string, string> = {}
      try {
        drafts = JSON.parse(localStorage.getItem(draftKey) ?? '{}')
      } catch {
        /* blocked or corrupt storage: start from the submitted text */
      }
      for (const f of files) {
        if (stale.includes(f.path)) continue
        const got = await api.reviewFile(id, f.path)
        if (got.text === null) continue
        const section = fileSection(review.diff, f.path)
        // Without a base the change cannot be shown, so the file is left to
        // the read-only diff below rather than opened with a guess.
        const original = section ? baseFromDiff(got.text, section) : null
        if (original === null) continue
        next.set(f.path, { original, head: got.text, now: drafts[f.path] ?? got.text })
      }
      editable = next
    } catch (e) {
      fileError = e instanceof Error ? e.message : String(e)
    } finally {
      loadingFiles = false
    }
  }

  function onFileChange(path: string, text: string) {
    const f = editable.get(path)
    if (!f) return
    editable.set(path, { ...f, now: text })
    editable = new Map(editable)
    // An unsent edit is the one piece of state the node does not hold, so it
    // is kept where it is least likely to be lost.
    try {
      const drafts: Record<string, string> = {}
      for (const [p, v] of editable) if (v.now !== v.head) drafts[p] = v.now
      if (Object.keys(drafts).length) localStorage.setItem(draftKey, JSON.stringify(drafts))
      else localStorage.removeItem(draftKey)
    } catch {
      /* blocked storage: the edit still stands in this tab */
    }
  }

  function discardEdits() {
    editable = new Map([...editable].map(([p, f]) => [p, { ...f, now: f.head }]))
    try {
      localStorage.removeItem(draftKey)
    } catch {
      /* nothing to clear */
    }
    editing = false
  }

  const target = $derived.by(() => {
    try {
      return JSON.parse(review?.target ?? 'null')
    } catch {
      return null
    }
  })
  const noun = $derived(review?.provider === 'gitlab' ? 'merge request' : 'pull request')
  /** The open change this review updates, or null when it opens a new one. */
  const change = $derived<{ number: number; url: string } | null>(target?.change ?? null)
  /**
   * Whether the review's title and body are only your summary. They describe
   * a new change when it has no description of its own, as every review did
   * before a revision could say otherwise.
   */
  const summaryOnly = $derived(change !== null || describe)
  const outputs = $derived<ReviewOutputs>({
    description: describe ? { title: descTitle, body: descBody } : undefined,
    comment: commenting && comment.trim() ? comment : undefined,
    draft: change === null ? intent.forge.draft : undefined,
  })
  const files = $derived.by(() => {
    try {
      return JSON.parse(review?.files ?? '[]') as { path: string; blob: string }[]
    } catch {
      return []
    }
  })
  const verdict = $derived(review ? reviewVerdict(review) : null)
  const checks = $derived(review ? reviewChecks(review) : [])
  const session = $derived(review ? store.sessions.get(review.session_id) : undefined)
  const reviewer = $derived(review?.review_session_id ? store.sessions.get(review.review_session_id) : undefined)
  const edited = $derived(
    review !== null &&
      (title !== review.title ||
        body !== review.body ||
        JSON.stringify(outputs) !==
          JSON.stringify({
            description: intent.forge.description,
            comment: intent.forge.comment,
            draft: change === null ? intent.forge.draft : undefined,
          })),
  )
  const authoritativeChecks = $derived(evidence?.checks ?? [])

  function inputsFor(run: CandidateEvidence['checks'][number]) {
    if (!run.inputs_json) return 'input identity was not recorded'
    try {
      return JSON.stringify(JSON.parse(run.inputs_json))
    } catch {
      return run.inputs_json
    }
  }

  /**
   * Record a verdict on one criterion, bound to the revision on the screen.
   * This is the only thing that can say a criterion was met: the checks
   * passing says the checks passed, and nothing more.
   */
  async function judgeCriterion(key: string) {
    if (!criteria) return
    busy = true
    criteriaError = null
    try {
      const res = await api.judgeCriterion(criteria.work_item_id, {
        criterion: key,
        verdict: criterionVerdict,
        note: criterionNote.trim() || undefined,
        candidate_id: criteria.candidate?.id ?? null,
        revision_id: revision?.id,
      })
      criteria = res.criteria
      criterionNote = ''
      judging = null
    } catch (e) {
      criteriaError = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }

  async function decide(verdict: 'approve' | 'reject' | 'revise') {
    if (!review || publishing) return
    busy = true
    error = null
    try {
      decided = true
      const res = await api.decideReview(id, {
        verdict,
        reason: verdict === 'approve' ? undefined : reason,
        title: verdict === 'approve' ? title : undefined,
        body: verdict === 'approve' ? body : undefined,
        outputs: verdict === 'approve' ? outputs : undefined,
        // An edit is a request for changes, never an approval of something
        // the operator changed: the agent applies it and resubmits.
        patch: verdict === 'revise' && patch ? patch : undefined,
        // What this screen is showing. If the agent resubmitted while the
        // verdict was being written, the node refuses it rather than
        // applying it to a screen that was never read. The revision is the
        // identity that carries: a resubmission may repeat the commit with
        // different requirements or prose.
        head_sha: review.head_sha,
        revision_id: revision?.id,
      })
      if (verdict !== 'approve') {
        try {
          localStorage.removeItem(draftKey)
        } catch {
          /* nothing to clear */
        }
      }
      await store.refetch()
      // The outcome is on the review and on the session, so a verdict that
      // finishes after the operator has gone to read something else leaves
      // them there rather than hauling them back.
      leaveAfterVerdict(router, {
        reviewId: id,
        from: openedFrom,
        fallback: res.published ? `/sessions/${review.session_id}` : '/',
      })
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
    }
  }
</script>

{#if !loaded}
  <div class="empty">Loading review…</div>
{:else if !review}
  <div class="banner crit">not found <b>· {error ?? 'no such review'}</b></div>
{:else if report}
  <ReportReview {report} />
{:else}
  <div class="head" class:stale={stale.length > 0}>
    <span class="bar"></span>
    <span class="mono">{formatAge(review.created_ms, clock.now)}</span>
    <span class="t">
      <em>{stale.length > 0 ? 'Changed since submit' : 'Review'}</em>
      {title || review.title}
      <small
        >{files.length} files · +{review.added} −{review.removed} · {review.channel}{review.claimed_ms
          ? ' · claimed'
          : ''}</small
      >
    </span>
  </div>

  <dl class="kv">
    <dt>Publishes</dt>
    <dd class="m">
      {#if change}
        updates <a href={change.url} target="_blank" rel="noreferrer">{noun} {change.number}</a>
      {:else}
        new {noun}{intent.forge.draft ? ' (draft)' : ''}
      {/if}
      → {target?.project} · {target?.branch} into {target?.base}
      {#if intent.rewrite}
        <span
          class="chip warn"
          title="the push replaces what the branch held at submit instead of adding to it"
          >rewrites history · replaces {intent.lease?.slice(0, 8)}</span
        >
      {/if}
    </dd>

    <dt>Session</dt>
    <dd class="m"><a href="/sessions/{review.session_id}">{review.session_id.slice(0, 8)}</a></dd>
  </dl>
  {#if !evidence}
    <div class="banner crit">
      verification evidence missing <b>· this review predates immutable candidate capture</b>
    </div>
  {:else}
    <section class="evidence">
      <div class="h4">
        Candidate evidence
        <b>{evidence.candidate.head_sha.slice(0, 12)} · {evidence.candidate.tree_sha?.slice(0, 12) ?? 'tree not recorded'}</b>
      </div>
      <div class="evidence-grid">
        <div class="evidence-panel">
          <h3>Requirements</h3>
          {#if requirements}
            <b>{requirements.title}</b>
            <p>{requirements.body || 'No additional requirement detail.'}</p>
            <a href="/work/{requirements.id}">open work item</a>
          {:else}
            <p class="missing">No work item was linked when this review was captured.</p>
          {/if}
        </div>
        <div class="evidence-panel">
          <h3>Pinned surrounding code</h3>
          {#if surroundingCode.length}
            {#each surroundingCode as context, index (index)}
              <details>
                <summary>{context.path}:{context.start_line}–{context.end_line}</summary>
                <pre>{context.text}</pre>
              </details>
            {/each}
          {:else}
            <p class="missing">No textual hunk context was captured.</p>
          {/if}
        </div>
        <div class="evidence-panel">
          <h3>Runtime evidence</h3>
          {#if authoritativeChecks.length}
            {#each authoritativeChecks as run (run.id)}
              <div class="run">
                <span class:ok={run.outcome === 'passed' || run.source_outcome === 'passed'} class:bad={run.outcome === 'failed' || run.outcome === 'not_runnable' || run.outcome === 'interrupted' || run.outcome === 'cancelled'}>
                  {run.outcome === 'not_runnable' ? 'not runnable' : run.outcome}{run.outcome === 'reused' ? ` · ${run.source_outcome ?? 'unknown source'}` : ''}
                </span>
                <code>{run.command ?? 'command not recorded'}</code>
                <code>{run.execution_image ?? 'image identity not recorded'}</code>
                <small>{inputsFor(run)}</small>
                {#if run.reused_from_id}<small>reused from {run.reused_from_id.slice(0, 8)}</small>{/if}
                <details>
                  <summary>log · {run.duration_ms == null ? 'unfinished' : `${Math.round(run.duration_ms / 1000)}s`}</summary>
                  <pre>{run.log || '(no retained output)'}</pre>
                </details>
              </div>
            {/each}
          {:else if target?.worktree}
            <p class="missing">Submitted from a harness you run yourself: the node ran no checks on this candidate.</p>
          {:else}
            <p class="missing">No required checks were configured for this candidate.</p>
          {/if}
        </div>
      </div>
      {#if evidence.demonstrations.length}
        <div class="demonstrations">
          <b>Curated demonstrations</b>
          {#each evidence.demonstrations as demo (demo.id)}
            <a href={`/docs/${demo.channel}/${demo.document_slug}`}>{demo.label}</a>
            {#if demo.stale}
              <span class="chip warn" title="the document has changed since this was attached">stale</span>
            {/if}
          {/each}
          <small>These linked documents are human-curated context; opening a review never executes them.</small>
        </div>
      {/if}
    </section>
  {/if}

  {#if criteria}
    <section class="criteria-block">
      <div class="h4">
        Acceptance criteria
        <b>{criteria.summary}</b>
      </div>
      {#if criteria.criteria.length === 0}
        <p class="missing">{criteria.absent ?? 'the brief states no success criteria'}</p>
      {:else}
        <p class="left">
          {whatIsLeft(criteria)} · the checks are this node's; whether the criterion was met is yours.
        </p>
        <ul>
          {#each criteria.criteria as c, i (i)}
            <li class={attention(c.coverage)}>
              <div class="line">
                <span class="chip {attention(c.coverage)}" title={COVERAGE[c.coverage].title}>{COVERAGE[c.coverage].label}</span>
                <span>{c.text}</span>
              </div>
              {#if c.duplicate}
                <small class="missing">
                  Duplicate criterion; <a href="/docs/{criteria.channel}/{criteria.brief_slug}">edit the brief to distinguish these lines.</a>
                </small>
              {/if}
              {#each c.links as l (l.index)}
                <small><code>{l.value}</code> · {linkSays(l)}</small>
              {/each}
              {#if c.links.length === 0}
                <small>Nothing points at this one, so nothing on this screen speaks to it.</small>
              {/if}
              {#if c.judgement}
                <small class="judged">
                  you judged it {c.judgement.verdict.replace('_', ' ')}{c.judgement.note ? ` · ${c.judgement.note}` : ''}
                </small>
              {:else if c.earlier_judgement}
                <small class="earlier">
                  judged {c.earlier_judgement.verdict.replace('_', ' ')}
                  {c.earlier_judgement.candidate_id ? 'for another attempt' : 'with no attempt named'} — it does not
                  settle this one
                </small>
              {/if}
              {#if c.duplicate}
                <button class="lnk" disabled>Judge it</button>
              {:else if judging === c.key}
                <div class="judge">
                  <select bind:value={criterionVerdict} aria-label="your verdict">
                    {#each VERDICTS as v (v)}<option value={v}>{v.replace('_', ' ')}</option>{/each}
                  </select>
                  <input placeholder="what you looked at" bind:value={criterionNote} />
                  <button class="btn p" onclick={() => judgeCriterion(c.key)} disabled={busy}>Record it</button>
                  <button class="lnk" onclick={() => (judging = null)} disabled={busy}>Cancel</button>
                </div>
              {:else}
                <button class="lnk" onclick={() => (judging = c.key)} disabled={busy}>
                  {c.judgement ? 'Judge it again' : 'Judge it'}
                </button>
              {/if}
            </li>
          {/each}
        </ul>
        {#if criteria.gaps.questions.length}
          <p class="missing">
            Still open: {criteria.gaps.questions.join(' · ')}
          </p>
        {/if}
      {/if}
      {#if criteriaError}<div class="banner crit">refused <b>· {criteriaError}</b></div>{/if}
    </section>
  {/if}

  {#if checks.length}
    <div class="checks">
      {#each checks as c (c.command)}<span class="chip ok">✓ {c.command} · {Math.round(c.ms / 1000)}s</span>{/each}
    </div>
  {/if}

  <dl class="prov">
    <div><dt>Model</dt><dd>{session ? `${session.model} · ${session.phase}` : '—'}</dd></div>
    <div><dt>Item</dt><dd>{#if session?.work_item_id}<a href="/work/{session.work_item_id}">{session.work_item_id.slice(0, 8)}</a>{:else}none{/if}</dd></div>
    <div><dt>Policy</dt><dd>{session?.policy_version != null ? `working-agreements v${session.policy_version}` : '—'}</dd></div>
    <div><dt>Reviewed by</dt><dd>{reviewer ? `${reviewer.model} · fresh session` : review.review_session_id ? 'fresh session' : 'no review model bound'}</dd></div>
    <div><dt>Commit</dt><dd>{review.head_sha.slice(0, 8)}</dd></div>
  </dl>

  {#if verdict}
    <div class="verdict" class:rc={verdict.verdict === 'request_changes'}>
      <span class="vbar"></span>
      <div class="in">
        <div class="who"><b>{verdict.verdict === 'approve' ? 'Approves' : 'Request changes'}</b> · {verdict.model} · read only the requirements and the diff{reviewer ? ` · ${Math.round(reviewer.tokens_used / 1000)}k tokens` : ''}</div>
        <div class="sum">{verdict.summary}</div>
        {#if verdict.findings?.length}
          <ul class="findings">
            {#each verdict.findings as f, i (i)}
              <li><span class="sev {f.severity ?? 'should'}">{f.severity ?? 'should'}</span><span class="path">{f.path ?? ''}{f.line ? `:${f.line}` : ''}</span><span>{f.note}</span></li>
            {/each}
          </ul>
        {/if}
      </div>
    </div>
  {:else if review.review_session_id}
    <div class="banner dim">a fresh session is reading this review <b>· its verdict lands here; yours decides</b></div>
  {/if}

  {#if review.state === 'revising'}
    <div class="banner ok">
      changes requested <b>· waiting on the agent to resubmit · {review.verdict_reason}</b>
    </div>
  {/if}
  {#if publishing}
    <div class="banner crit">
      publication outcome requires reconciliation <b>· this review may have reached the forge; do not approve, revise, or reject it again</b>
    </div>
  {/if}

  {#if stale.length > 0}
    <div class="banner crit">
      changed since submit <b>· {stale.join(', ')} · approve is disabled; ask the agent to resubmit</b>
    </div>
  {/if}

  {#if summaryOnly}
    <div class="h4">Summary <b>for you · not sent to the forge</b></div>
    <div class="summary">
      <b>{review.title}</b>
      <p>{review.body}</p>
    </div>
  {:else}
    <div class="h4">
      Title and body <b>{surface.phone ? 'edited on the desktop' : 'edit before approving if you want to'}</b>
    </div>
    <input class="edit" bind:value={title} disabled={busy || publishing || surface.phone} />
    <textarea class="edit body" bind:value={body} use:autogrow={body} disabled={busy || publishing || surface.phone}></textarea>
  {/if}

  <div class="h4">On the forge <b>{surface.phone ? 'edited on the desktop' : 'what approval sends besides the commits'}</b></div>
  <label class="toggle">
    <input type="checkbox" bind:checked={describe} disabled={busy || publishing || surface.phone} />
    {change ? `Replace the ${noun}'s title and description` : `Describe the ${noun} separately from the summary`}
  </label>
  {#if describe}
    <input class="edit" bind:value={descTitle} placeholder="title" disabled={busy || publishing || surface.phone} />
    <textarea class="edit body" bind:value={descBody} use:autogrow={descBody} disabled={busy || publishing || surface.phone}></textarea>
  {/if}
  <label class="toggle">
    <input type="checkbox" bind:checked={commenting} disabled={busy || publishing || surface.phone} />
    Comment on the {noun}
  </label>
  {#if commenting}
    <textarea class="edit body" bind:value={comment} use:autogrow={comment} disabled={busy || publishing || surface.phone}></textarea>
  {/if}
  {#if change && !describe && !(commenting && comment.trim())}
    <div class="note dim">Approving only pushes; the {noun}'s text is left as it is.</div>
  {/if}
  {#if edited}
    <div class="note">Edited. Approving publishes what is written here, not what was submitted.</div>
  {/if}

  <div class="h4">Files <b>{files.length}</b></div>
  {#if !surface.phone}
    <div class="files">
      {#each files as f (f.path)}
        <div class:moved={stale.includes(f.path)}>
          <span>{f.path}</span>
          <span class={stale.includes(f.path) ? 'bad' : 'ok'}
            >{stale.includes(f.path) ? 'changed since submit' : 'unchanged'}</span
          >
        </div>
      {/each}
    </div>
  {/if}

  <!-- On the phone the file list is the diff: it decides most reviews, and each
       file opens to its hunks when it does not. -->
  {#if editing}
    {#if loadingFiles}
      <div class="empty">Opening the files…</div>
    {:else if fileError}
      <div class="banner crit">could not open the files <b>· {fileError}</b></div>
    {:else if editable.size === 0}
      <div class="banner dim">
        nothing here can be edited <b>· every file changed since submit, or is new or binary</b>
      </div>
    {:else}
      {#each [...editable] as [path, f] (path)}
        <div class="filehead">
          <span class="p">{path}</span>
          {#if f.now !== f.head}<span class="chip warn">edited</span>{/if}
        </div>
        {#if DiffEditor}
          <DiffEditor {path} original={f.original} head={f.head} onchange={onFileChange} />
        {/if}
      {/each}
      {#if stale.length > 0}
        <p class="note">
          {stale.length} file{stale.length === 1 ? '' : 's'} changed since submit and cannot be
          edited; {stale.join(', ')}
        </p>
      {/if}
    {/if}
  {:else}
    <Diff diff={review.diff} perFile={surface.phone} />
  {/if}

  {#if !surface.phone}
    <div class="editbar">
      {#if !editing}
        <button class="btn" disabled={busy || publishing} onclick={startEditing}>Edit the diff</button>
        <span class="note">Edits go back as a request for changes; the agent applies them.</span>
      {:else}
        <button class="btn" disabled={busy || publishing} onclick={discardEdits}>Discard edits</button>
        <span class="note"
          >{editedFiles === 0
            ? 'No edits yet.'
            : `${editedFiles} file${editedFiles === 1 ? '' : 's'} edited · sent with Request changes`}</span
        >
      {/if}
    </div>
  {:else}
    <p class="note">Editing a diff needs a keyboard and a wide screen — open this on the desktop.</p>
  {/if}

  {#if error}
    <div class="banner crit">refused <b>· {error}</b></div>
  {/if}

  <div class="verdict">
    <button class="btn p" disabled={busy || publishing || stale.length > 0} onclick={() => decide('approve')}>
      Approve and publish
    </button>
    <input
      bind:value={reason}
      placeholder="What to change, or why you are rejecting — goes back to the agent"
      disabled={busy || publishing}
    />
    <button class="btn" disabled={busy || publishing || !reason.trim()} onclick={() => decide('revise')}>
      {editedFiles > 0 ? 'Send edits and request changes' : 'Request changes'}
    </button>
    <button class="btn d" disabled={busy || publishing || !reason.trim()} onclick={() => decide('reject')}>
      Reject
    </button>
  </div>
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
  .head.stale {
    background: linear-gradient(90deg, var(--wash-crit), var(--s1) 42%);
  }
  .head.stale .bar {
    background: var(--crit);
  }
  .t {
    font-weight: 500;
    min-width: 0;
  }
  .t em {
    font-style: normal;
    font-weight: 400;
    color: var(--wait);
  }
  .head.stale .t em {
    color: var(--crit);
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
  }
  .kv dd.m {
    font: 12.5px var(--mono);
    color: var(--ink2);
  }
  .evidence {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }
  .evidence-grid {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 10px;
  }
  .evidence-panel {
    min-width: 0;
    padding: 10px;
    border-radius: 4px;
    background: var(--s1);
    font-size: 12.5px;
  }
  .evidence-panel h3 {
    margin: 0 0 7px;
    font: 11px var(--mono);
    color: var(--dim);
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
  .evidence-panel p {
    margin: 5px 0;
    white-space: pre-wrap;
  }
  .evidence-panel details,
  .run {
    border-top: 1px solid var(--rule);
    padding: 6px 0;
  }
  .evidence-panel details:first-of-type,
  .run:first-of-type {
    border-top: 0;
    padding-top: 0;
  }
  .evidence-panel summary {
    cursor: pointer;
    color: var(--ink2);
    font: 11.5px var(--mono);
  }
  .evidence pre {
    max-height: 210px;
    margin: 6px 0 0;
    overflow: auto;
    white-space: pre;
    font: 11px/1.45 var(--mono);
  }
  .run {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .run > span {
    font: 11px var(--mono);
    color: var(--dim);
  }
  .run > span.ok {
    color: var(--ok);
  }
  .run > span.bad {
    color: var(--crit);
  }
  .run code,
  .run small {
    overflow-wrap: anywhere;
    color: var(--ink2);
  }
  .run small,
  .missing,
  .demonstrations small {
    color: var(--dim);
  }
  .demonstrations {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 12.5px;
  }
  .demonstrations a {
    font: 12px var(--mono);
  }
  .checks {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
  }
  .chip.ok {
    background: var(--wash-ok);
    color: var(--ok);
  }
  .criteria-block {
    display: flex;
    flex-direction: column;
    gap: 9px;
  }
  .criteria-block .left {
    margin: 0;
    color: var(--ink2);
    font-size: 13px;
  }
  .criteria-block ul {
    margin: 0;
    padding: 0;
    list-style: none;
    display: grid;
    gap: 7px;
  }
  .criteria-block li {
    background: var(--s1);
    border-radius: 4px;
    border-left: 2px solid var(--dim);
    padding: 8px 11px;
    display: grid;
    gap: 4px;
    font-size: 13.5px;
    line-height: 1.45;
  }
  .criteria-block li.gap {
    border-left-color: var(--wait);
  }
  .criteria-block li.fail {
    border-left-color: var(--crit);
  }
  .criteria-block li.settled {
    border-left-color: var(--ok);
  }
  .criteria-block .line {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
  }
  .criteria-block small {
    color: var(--dim);
    font: 11.5px var(--mono);
  }
  .criteria-block small.judged {
    color: var(--ok);
  }
  .criteria-block small.earlier {
    color: var(--wait);
  }
  .criteria-block code {
    font: 11.5px var(--mono);
    color: var(--ink2);
  }
  .criteria-block .judge {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
  }
  .criteria-block input,
  .criteria-block select {
    background: var(--s2);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 6px 9px;
    font: 12.5px var(--sans);
  }
  .criteria-block input {
    flex: 1;
    min-width: 140px;
  }
  .criteria-block button {
    justify-self: start;
  }
  .chip.gap,
  .chip.wait {
    background: var(--wash-wait);
    color: var(--wait);
  }
  .chip.gap::before,
  .chip.wait::before {
    background: var(--wait);
  }
  .chip.fail {
    background: var(--wash-crit);
    color: var(--crit);
  }
  .chip.fail::before {
    background: var(--crit);
  }
  .chip.settled {
    background: var(--wash-ok);
    color: var(--ok);
  }
  .prov {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 10px 16px;
    background: var(--s1);
    border-radius: 4px;
    padding: 10px 14px;
    margin: 0;
  }
  .prov div {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .prov dt {
    font: 11px var(--mono);
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--dim);
  }
  .prov dd {
    margin: 0;
    font: 12.5px var(--mono);
    color: var(--ink2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .verdict {
    display: grid;
    grid-template-columns: 3px minmax(0, 1fr);
    gap: 0 14px;
    background: var(--s1);
    border-radius: 4px;
    overflow: hidden;
  }
  .vbar {
    background: var(--acc);
  }
  .verdict.rc .vbar {
    background: var(--wait);
  }
  .verdict .in {
    padding: 10px 14px 12px 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .who {
    font: 12px var(--mono);
    color: var(--dim);
  }
  .who b {
    font-weight: 500;
    color: var(--acc);
  }
  .verdict.rc .who b {
    color: var(--wait);
  }
  .sum {
    font-size: 13.5px;
    max-width: 70ch;
  }
  .findings {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin: 2px 0 0;
    padding: 0;
    list-style: none;
  }
  .findings li {
    display: grid;
    grid-template-columns: 64px 170px minmax(0, 1fr);
    gap: 10px;
    font-size: 12.5px;
  }
  .sev {
    font: 11px var(--mono);
    color: var(--wait);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .sev.nit {
    color: var(--dim);
  }
  .sev.blocking {
    color: var(--crit);
  }
  .path {
    font: 12px var(--mono);
    color: var(--ink2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  @media (max-width: 700px) {
    .evidence-grid {
      grid-template-columns: 1fr;
    }
    .findings li {
      grid-template-columns: 64px 1fr;
    }
    .path {
      grid-column: 2;
    }
  }
  .edit {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 9px 11px;
    font: 13.5px var(--sans);
  }
  .edit.body {
    min-height: 110px;
    max-height: 70vh;
    resize: vertical;
    font-family: var(--sans);
  }
  .note {
    font-size: 12.5px;
    color: var(--wait);
  }
  .note.dim {
    color: var(--dim);
  }
  .summary {
    background: var(--s1);
    border-radius: 4px;
    padding: 9px 11px;
    font-size: 13.5px;
  }
  .summary p {
    margin: 5px 0 0;
    white-space: pre-wrap;
    color: var(--ink2);
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--ink2);
  }
  .kv .chip.warn {
    background: var(--wash-wait);
    color: var(--wait);
    margin-left: 6px;
  }
  .editbar {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .editbar .note {
    color: var(--dim);
  }
  .filehead {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
  }
  .filehead .p {
    font: 12.5px var(--mono);
    color: var(--ink2);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .filehead .chip.warn {
    background: var(--wash-wait);
    color: var(--wait);
  }
  .files {
    background: var(--s1);
    border-radius: 4px;
    font: 12.5px var(--mono);
    overflow: hidden;
  }
  .files div {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    padding: 6px 12px;
    border-top: 1px solid var(--rule);
  }
  .files div:first-child {
    border-top: 0;
  }
  .files .ok {
    color: var(--dim);
  }
  .files .bad {
    color: var(--crit);
  }
  .verdict {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    align-items: center;
    border-top: 1px solid var(--rule);
    padding-top: 14px;
  }
  .verdict input {
    flex: 1;
    min-width: 200px;
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--sans);
  }
</style>
