<script lang="ts">
  import ReportReview from '../components/ReportReview.svelte'
  import Diff from '../components/Diff.svelte'
  import { splitDiff } from '../lib/diff'
  import CommitMessage from '../components/CommitMessage.svelte'
  import ShownWork from '../components/ShownWork.svelte'
  import CheckDetail from '../components/CheckDetail.svelte'
  import { tick } from 'svelte'
  import CloseTask from '../components/CloseTask.svelte'
  import { api } from '../lib/api'
  import { clock } from '../lib/clock.svelte'
  import { formatAge } from '../lib/format'
  import { isNotFound } from '../lib/load'
  import { router } from '../lib/router.svelte'
  import { store } from '../lib/store.svelte'
  import { nodeLabel } from '../lib/nodes'
  import { autogrow } from '../lib/autogrow'
  import { renderMessage } from '../lib/markdown'
  import { surface } from '../lib/surface.svelte'
  import {
    reviewChecks,
    reviewVerdict,
    type CandidateEvidence,
    type Criteria,
    type OwnerDetail,
    type PinnedRequirements,
    type Review,
    type ReviewAuthorship,
    type CandidateCheckRun,
    type ForgeState,
    type ReviewIntent,
    type ReviewOutputs,
    type ReviewPublication,
    type ReviewRevisionRef,
    type SinceReviewed,
    type ShownWork as ShownWorkItem,
    type Verdict,
    type AiFinding,
    type ReviewerView,
  } from '../lib/types'
  import { buildPatch, fileSection, openForEdit } from '../lib/patch'
  import { isNarrativeReport } from '../lib/reports'
  import { leaveAfterVerdict } from '../lib/verdict-nav'
  import { draftedMessage, draftForRevision, ReviewDraftSync, type DraftState } from '../lib/reviewDraft'
  import { proposedMessage as proposedCommitMessage, shipping, shippedMessage } from '../lib/shipping'
  import type { ReviewCommit, ReviewDraftFields } from '../lib/types'
  import {
    CLOSED_STATES,
    basisLabel,
    checksState,
    composerLabel,
    publishedChange,
    publishedLabel,
    rereadDelay,
    rereadReviewer,
    reviewerControl,
    startedHow,
    unavailable,
  } from '../lib/verdicts'
  import { openAiRequest, sentReason, withFinding } from '../lib/reviewerChanges'
  import { COVERAGE, FINDING, VERDICTS, attention, linkSays, whatIsLeft } from '../lib/criteria'
  import { failuresOf, outcomeLabel, runDuration, runTone, runsSummary } from '../lib/checkRuns'

  let { id }: { id: string } = $props()

  let review = $state<Review | null>(null)
  /** The revision this screen is showing; the verdict names it. */
  let revision = $state<ReviewRevisionRef | null>(null)
  let stale = $state<string[]>([])
  /** The node that owns a review mirrored here; null when this node does. */
  let remoteOwner = $state<string | null>(null)
  let ownerDetail = $state<OwnerDetail | null>(null)
  let evidence = $state<CandidateEvidence | null>(null)
  let shownWork = $state<ShownWorkItem[]>([])
  let authorship = $state<ReviewAuthorship | null>(null)
  let publication = $state<ReviewPublication | null>(null)
  /** Where the published request stands on the forge, as last followed. */
  let forgeState = $state<ForgeState | null>(null)
  /** Why no reviewer is reading it, as the node last found. */
  let reviewerReason = $state<string | null>(null)
  let reviewerStarting = $state(false)
  /** What became of the latest revision's reviewer, as the node last said. */
  let reviewerView = $state<ReviewerView | null>(null)
  let requirements = $state<PinnedRequirements | null>(null)
  /** The pinned item's criteria against this revision's candidate, if any. */
  let criteria = $state<Criteria | null>(null)
  /** The criterion whose verdict is being given, by key. */
  let judging = $state<string | null>(null)
  let criterionVerdict = $state<Verdict>('met')
  let criterionNote = $state('')
  let criteriaError = $state<string | null>(null)
  /** What changed since your last verdict, and which diff is showing. */
  let sinceReviewed = $state<SinceReviewed | null>(null)
  let diffView = $state<'since' | 'full'>('full')
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
  /** The squashed commit's message, and what it was before you edited it. */
  let message = $state('')
  let proposedMessage = ''
  /**
   * Under `keep`: the message of each of the agent's commits open for
   * editing, by sha. A commit not here ships as the agent wrote it.
   */
  let commitMessages = $state<Record<string, string>>({})
  /** The branch a new change is pushed to. */
  let branchName = $state('')
  let busy = $state(false)
  let error = $state<string | null>(null)
  /** The change the operator opened by hand, while "Published at…" is open;
   * `null` while it is closed. */
  let reconcileChange = $state<string | null>(null)
  let loaded = $state(false)
  /** The node said there is no such review, as opposed to failing to say. */
  let missing = $state(false)
  /** Bumped by Retry to read the review again. */
  let attempt = $state(0)
  /** Unsent words, held by the node so another device or a reload finds them. */
  let draftSync: ReviewDraftSync | null = null
  let draftState = $state<DraftState>('clean')
  /** The draft on the screen was written against an earlier revision. */
  let draftFromEarlier = $state(false)
  /** What another device saved, while the two disagree. */
  let draftTheirs = $state<ReviewDraftFields | null>(null)
  /**
   * The squash message of a draft written against an earlier revision, held
   * aside rather than put over this revision's, until the operator chooses.
   */
  let draftMessage = $state<string | null>(null)
  /** The route this review was opened from; a verdict goes back to it. */
  let openedFrom: string | null = null
  const report = $derived(review && isNarrativeReport(review) ? review : null)

  /** One of the agent's commits' messages, as it wrote it. */
  function writtenMessage(c: ReviewCommit): string {
    return c.body?.trim() ? `${c.subject}\n\n${c.body.trim()}` : c.subject
  }

  /** The commit messages edited away from what the agent wrote, by sha. */
  function editedMessages(): Record<string, string> {
    const edited: Record<string, string> = {}
    for (const c of intent.commits ?? []) {
      const now = commitMessages[c.sha]
      if (now !== undefined && now.trim() !== writtenMessage(c).trim()) edited[c.sha] = now.trim()
    }
    return edited
  }

  function editCommit(c: ReviewCommit) {
    commitMessages = { ...commitMessages, [c.sha]: writtenMessage(c) }
  }

  function revertCommit(sha: string) {
    const { [sha]: _, ...rest } = commitMessages
    commitMessages = rest
  }

  $effect(() => {
    void id
    void attempt
    loaded = false
    missing = false
    error = null
    openedFrom = router.previous
    draftSync?.stop()
    draftSync = null
    draftState = 'clean'
    draftFromEarlier = false
    draftTheirs = null
    draftMessage = null
    Promise.all([api.review(id), api.reviewDraft(id).catch(() => ({ draft: null }))])
      .then(([d, held]) => {
        review = d.review
        revision = d.revision
        stale = d.stale
        remoteOwner = d.remote_owner ?? null
        ownerDetail = d.owner_detail ?? null
        evidence = d.evidence
        shownWork = d.shown_work ?? []
        authorship = d.authorship ?? null
        publication = d.publication ?? null
        forgeState = d.forge_state ?? null
        reviewerReason = d.review_session_reason ?? null
        reviewerStarting = d.review_session_starting ?? false
        reviewerView = d.reviewer ?? null
        requirements = d.requirements
        criteria = d.criteria
        sinceReviewed = d.since_reviewed ?? null
        // A resubmission opens on what answered your feedback; the full diff
        // is one tap away.
        diffView = sinceReviewed?.diff ? 'since' : 'full'
        title = d.review.edited_title ?? d.review.title
        body = d.review.edited_body ?? d.review.body
        intent = d.intent ?? { forge: {} }
        describe = intent.forge.description !== undefined
        descTitle = intent.forge.description?.title ?? ''
        descBody = intent.forge.description?.body ?? ''
        commenting = intent.forge.comment !== undefined
        comment = intent.forge.comment ?? ''
        proposedMessage = proposedCommitMessage(intent)
        message = proposedMessage
        commitMessages = {}
        try {
          branchName = JSON.parse(d.review.target)?.branch ?? ''
        } catch {
          branchName = ''
        }
        // What the operator left here, on this device or another, comes back
        // over what the agent submitted.
        if (held.draft) {
          const applied = draftForRevision(held.draft, d.revision?.id, proposedMessage)
          applyDraft(applied.fields)
          draftFromEarlier = applied.fromEarlier
          draftMessage = squashing ? applied.staleMessage : null
        }
        draftSync = new ReviewDraftSync(id, d.revision?.id, (state) => {
          draftState = state
          draftTheirs = state === 'conflict' ? (draftSync?.conflict?.draft ?? {}) : null
        })
        draftSync.loaded(held.draft, draftFields())
        loaded = true
      })
      .catch((e) => {
        error = e instanceof Error ? e.message : String(e)
        missing = isNotFound(e)
        loaded = true
      })
    // Release on navigating away. The node's sweeper covers a client that
    // vanishes without getting here.
    return () => {
      if (!decided && !closed) void api.releaseReview(id).catch(() => {})
    }
  })

  let decided = $state(false)

  function draftFields(): ReviewDraftFields {
    return {
      reason,
      title,
      body,
      describe,
      descTitle,
      descBody,
      commenting,
      comment,
      message: draftedMessage(message, proposedMessage),
      branch: branchName,
      messages: editedMessages(),
    }
  }

  function applyDraft(d: ReviewDraftFields) {
    if (d.reason !== undefined) reason = d.reason
    if (d.title !== undefined) title = d.title
    if (d.body !== undefined) body = d.body
    if (d.describe !== undefined) describe = d.describe
    if (d.descTitle !== undefined) descTitle = d.descTitle
    if (d.descBody !== undefined) descBody = d.descBody
    if (d.commenting !== undefined) commenting = d.commenting
    if (d.comment !== undefined) comment = d.comment
    if (d.message !== undefined) message = d.message
    if (d.branch !== undefined) branchName = d.branch
    // Written against an earlier revision, an edit of a commit this one no
    // longer has is dropped.
    if (d.messages !== undefined) {
      const listed = new Set((intent.commits ?? []).map((c) => c.sha))
      commitMessages = Object.fromEntries(Object.entries(d.messages).filter(([sha]) => listed.has(sha)))
    }
  }

  // Every change to what the operator is writing is saved to the node after
  // a pause; `draftSync` skips what it already sent.
  $effect(() => {
    const fields = draftFields()
    if (loaded) draftSync?.edit(fields)
  })

  function takeTheirs() {
    if (!draftSync) return
    applyDraft(draftSync.takeTheirs())
  }

  function keepMine() {
    void draftSync?.keepMine(draftFields())
  }

  /** Settle a squash message drafted against an earlier revision. */
  function settleDraftMessage(use: boolean) {
    if (use && draftMessage !== null) message = draftMessage
    draftMessage = null
  }

  // Editing the diff, desktop only. The phone directs; it does not edit.
  let editing = $state(false)
  // CodeMirror is most of a megabyte and the phone never opens it, so it is
  // fetched when the operator asks to edit rather than on every page load.
  let DiffEditor = $state<typeof import('../components/DiffEditor.svelte').default | null>(null)
  let loadingFiles = $state(false)
  let fileError = $state<string | null>(null)
  /** path → { original: before the change, head: as submitted, now: edited } */
  let editable = $state<Map<string, { original: string; head: string; now: string }>>(new Map())
  /** The files the editor left out, shown read-only so the file list still reaches them. */
  let readOnly = $state<{ path: string; why: string }[]>([])

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
  /** Decided and done: nothing on this screen is sent any more. */
  const closed = $derived(review !== null && (CLOSED_STATES as readonly string[]).includes(review.state))
  /** Where the approval landed, when it reached the forge. */
  const landed = $derived(
    review?.state === 'approved'
      ? publishedChange(publication?.latest?.outcome === 'published' ? publication.latest.url : null) ??
          publishedChange(review.publish_result)
      : null,
  )
  const readiness = $derived(remoteOwner ? null : (publication?.readiness ?? null))
  /** The verdict whose reason is being written, if one is. */
  let composing = $state<'revise' | 'reject' | null>(null)
  let reasonInput = $state<HTMLTextAreaElement | null>(null)
  const conditions = $derived({ busy, publishing, stale, reason, readiness })
  const approveBlocked = $derived(unavailable('approve', conditions))
  const sendBlocked = $derived(composing ? unavailable(composing, conditions) : null)

  /** Open the reason composer for a verdict that needs one. */
  function compose(action: 'revise' | 'reject') {
    composing = action
    queueMicrotask(() => reasonInput?.focus())
  }

  /** The AI reviewer's findings in the composer, each ticked while its item is in the text. */
  let aiFindings = $state<{ finding: AiFinding; on: boolean }[] | null>(null)

  /** Open Request changes on the AI reviewer's summary and findings, for you to edit and send. */
  function requestTheseChanges() {
    if (!verdict) return
    const findings = verdict.findings ?? []
    const opened = openAiRequest(reason, { summary: verdict.summary, findings, at: verdict.at_ms })
    reason = opened.text
    aiFindings = findings.map((finding, i) => ({ finding, on: opened.ticked[i] }))
    compose('revise')
  }

  function tickFinding(i: number, on: boolean) {
    if (!aiFindings) return
    aiFindings[i].on = on
    reason = withFinding(
      reason,
      aiFindings[i].finding,
      on,
      aiFindings.map((f) => f.finding),
    )
  }

  function closeComposer() {
    composing = null
    aiFindings = null
  }

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
    const left: { path: string; why: string }[] = []
    try {
      let drafts: Record<string, string> = {}
      try {
        drafts = JSON.parse(localStorage.getItem(draftKey) ?? '{}')
      } catch {
        /* blocked or corrupt storage: start from the submitted text */
      }
      for (const f of files) {
        if (stale.includes(f.path)) {
          left.push({ path: f.path, why: 'changed since submit' })
          continue
        }
        const got = await api.reviewFile(id, f.path)
        // Without a base the change cannot be shown, so the file is shown
        // read-only rather than opened with a guess.
        const opened = openForEdit(got.text, fileSection(review.diff, f.path))
        if ('why' in opened) {
          left.push({ path: f.path, why: opened.why })
          continue
        }
        next.set(f.path, { ...opened, now: drafts[f.path] ?? opened.head })
      }
      editable = next
      readOnly = left
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
  /** The forge had no branches: approval pushes the base itself and opens no change. */
  const initial = $derived(target?.initial === true)
  /**
   * Whether the review's title and body are only your summary. They describe
   * a new change when it has no description of its own, as every review did
   * before a revision could say otherwise.
   */
  const summaryOnly = $derived(change !== null || describe)
  /** Whether the reviewed tree ships as one commit rather than the agent's. */
  const squashing = $derived(intent.squash_onto !== undefined)
  /** Under `keep`: the commit messages you changed, by sha. */
  const reworded = $derived.by(editedMessages)
  /** Whether you can change the commit messages here. */
  const rewordable = $derived(!squashing && !remoteOwner && !closed && !surface.phone)
  const outputs = $derived<ReviewOutputs>({
    description: describe ? { title: descTitle, body: descBody } : undefined,
    comment: commenting && comment.trim() ? comment : undefined,
    draft: change === null ? intent.forge.draft : undefined,
    commit:
      squashing && (intent.forge.commit !== undefined || message.trim() !== proposedMessage)
        ? message.trim()
        : undefined,
    branch:
      change === null && !initial && branchName.trim() && branchName.trim() !== target?.branch
        ? branchName.trim()
        : undefined,
    messages: !squashing && Object.keys(reworded).length ? reworded : undefined,
  })
  /**
   * Whether the agent's own commit ships, and which signatures a new one
   * drops, judged by the message the node will squash with: only an edited
   * commit message changes it.
   */
  const ships = $derived(shipping(intent, shippedMessage(outputs, intent), reworded))
  const diffFiles = $derived(splitDiff(review?.diff ?? ''))
  const files = $derived.by(() => {
    try {
      return JSON.parse(review?.files ?? '[]') as { path: string; blob: string }[]
    } catch {
      return []
    }
  })
  const verdict = $derived(review ? reviewVerdict(review) : null)
  const checks = $derived(review ? reviewChecks(review) : [])
  /** Checks the node tried to run on an external candidate and could not. */
  const unrun = $derived(checks.find((c) => c.outcome === 'could_not_run'))
  /** The node is running an external candidate's checks; they gate nothing. */
  const checksRunning = $derived(checksState(checks) === 'running')
  /** A reviewer is waiting on them: only when the channel binds a review model. */
  const reviewerWaits = $derived(reviewerReason?.startsWith('waiting for') ?? false)
  const session = $derived(review?.session_id ? store.sessions.get(review.session_id) : undefined)
  const reviewer = $derived(review?.review_session_id ? store.sessions.get(review.review_session_id) : undefined)
  // The reviewer's id and state alone: the session row changes on every
  // update it streams, and the poll below must not be put off by each one.
  const liveReviewerId = $derived(reviewer?.id)
  const liveReviewerState = $derived(reviewer?.state)
  const control = $derived(review ? reviewerControl(review.state, remoteOwner ? null : reviewerView, reviewerStarting) : null)
  /** The verdict on the card is on an earlier revision than this one. */
  const verdictRevision = $derived(
    reviewerView?.verdict_revision != null && reviewerView.verdict_revision !== reviewerView.revision
      ? reviewerView.verdict_revision
      : null,
  )
  /** Review again replaces the verdict shown; it asks first. */
  let confirmingAgain = $state(false)
  // A reviewer that ends, or one the node had not recorded yet, is read
  // again, so its verdict or its failure shows without a reload.
  /** Polls in a row that failed: each schedules the next, further off. */
  let pollFailures = $state(0)
  $effect(() => {
    const when = rereadReviewer(
      control,
      reviewerView,
      reviewerStarting,
      liveReviewerId && liveReviewerState ? { id: liveReviewerId, state: liveReviewerState } : null,
    )
    if (!when) return
    const delay = rereadDelay(when, pollFailures)
    if (delay === 0) {
      void refreshReviewer()
    } else {
      const t = setTimeout(() => void refreshReviewer(), delay)
      return () => clearTimeout(t)
    }
  })
  const edited = $derived(
    review !== null &&
      (title !== review.title ||
        body !== review.body ||
        JSON.stringify(outputs) !==
          JSON.stringify({
            description: intent.forge.description,
            comment: intent.forge.comment,
            draft: change === null ? intent.forge.draft : undefined,
            commit: squashing ? intent.forge.commit : undefined,
            branch: undefined,
          })),
  )
  const authoritativeChecks = $derived(evidence?.checks ?? [])
  /** The run whose detail is open, if one is. */
  let openRun = $state<CandidateCheckRun | null>(null)
  /** Failed runs the operator folded away; a failed run opens unfolded. */
  let folded = $state<Set<string>>(new Set())

  function fold(id: string) {
    const next = new Set(folded)
    if (next.has(id)) next.delete(id)
    else next.add(id)
    folded = next
  }

  /** What a failed run says failed: its recorded failures, else the end of its log. */
  function whatFailed(run: CandidateCheckRun): string {
    return failuresOf(run) || run.log.trimEnd().split('\n').slice(-12).join('\n')
  }

  /** Scroll to a file's changes, in the diff or the editor, whichever is showing. */
  async function showFile(path: string) {
    if (!editing && diffView === 'since' && !sinceReviewed?.files?.some((f) => f.path === path)) {
      diffView = 'full'
      await tick()
    }
    const at = CSS.escape(path)
    document
      .querySelector(editing ? `.filehead[data-path="${at}"]` : `.diff [data-path="${at}"]`)
      ?.scrollIntoView({ block: 'start', behavior: 'smooth' })
  }

  /** The title of shown work on this review, for a finding that cites it. */
  function shownTitle(id: string): string | null {
    return shownWork.find((w) => w.id === id)?.title ?? null
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
    draftSync?.stop()
    try {
      decided = true
      const res = await api.decideReview(id, {
        verdict,
        // Sent on from the AI review, it stays marked as such.
        reason: verdict === 'approve' ? undefined : sentReason(reason),
        title: verdict === 'approve' ? title : undefined,
        body: verdict === 'approve' ? body : undefined,
        // A mirrored review's outputs are the owner's revision's own; this
        // screen never read them, so it sends no edit of them.
        outputs: verdict === 'approve' && !remoteOwner ? outputs : undefined,
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
        fallback: res.published && review.session_id ? `/sessions/${review.session_id}` : '/',
      })
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      // Nothing was decided, so what was written is still a draft.
      draftSync?.resume()
      draftSync?.edit(draftFields())
      // A refused approval may have left a publication record; show how it
      // ended and what can be done, not only the error.
      if (verdict === 'approve') await reread()
    } finally {
      busy = false
    }
  }

  /** The review and its publication record, as they stand now. */
  async function reread() {
    try {
      const d = await api.review(id)
      review = d.review
      publication = d.publication ?? null
      forgeState = d.forge_state ?? null
      reviewerReason = d.review_session_reason ?? null
      reviewerStarting = d.review_session_starting ?? false
      reviewerView = d.reviewer ?? null
    } catch {
      /* the error already shown is the one that matters */
    }
  }

  /** The reviewer alone, as it stands now: what the page polls while one runs. A review decided meanwhile is read again whole. */
  async function refreshReviewer() {
    if (!review) return
    try {
      const d = await api.reviewSession(id)
      if (d.state !== review.state) return await reread()
      review = { ...review, review_session_id: d.review_session_id, ai_verdict_json: d.ai_verdict_json }
      reviewerReason = d.review_session_reason ?? null
      reviewerStarting = d.review_session_starting
      reviewerView = d.reviewer
      pollFailures = 0
    } catch {
      // Asked again on the next poll, a little later each time.
      pollFailures++
    }
  }

  /** Start a fresh reviewer on this review; the node says why when none starts. */
  async function startReviewer() {
    if (!review) return
    busy = true
    error = null
    try {
      await api.startReviewer(id)
      await store.refetch()
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
    } finally {
      busy = false
      await reread()
    }
  }

  /** Retry the latest publication exactly as it was approved. Not a second
   * approval: nothing on this screen is sent, the node reads what was
   * approved back from its journal. */
  async function retry() {
    const latest = publication?.latest
    if (!review || !latest) return
    busy = true
    error = null
    try {
      decided = true
      const res = await api.recoverPublication(id, latest.id)
      await store.refetch()
      leaveAfterVerdict(router, {
        reviewId: id,
        from: openedFrom,
        fallback: res.published && review.session_id ? `/sessions/${review.session_id}` : '/',
      })
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      await reread()
    } finally {
      busy = false
    }
  }

  /** Settle the latest publication as published at the change the operator
   * opened by hand. The node checks it against the forge, and refuses a
   * change that is not exactly this publication's. */
  async function reconcile() {
    const latest = publication?.latest
    const change = reconcileChange?.trim()
    if (!review || !latest || !change) return
    busy = true
    error = null
    try {
      decided = true
      const res = await api.reconcilePublication(id, latest.id, change)
      reconcileChange = null
      await store.refetch()
      leaveAfterVerdict(router, {
        reviewId: id,
        from: openedFrom,
        fallback: res.published && review.session_id ? `/sessions/${review.session_id}` : '/',
      })
    } catch (e) {
      error = e instanceof Error ? e.message : String(e)
      await reread()
    } finally {
      busy = false
    }
  }

  const draftLine = $derived(
    draftState === 'saving'
      ? 'saving draft…'
      : draftState === 'saved'
        ? `draft saved${draftFromEarlier ? ' · written against an earlier revision' : ''}`
        : draftState === 'unsaved'
          ? 'draft not saved · the node could not be reached; it saves on your next edit'
          : '',
  )

  const CLOSED_LABEL: Record<string, string> = {
    approved: 'Approved',
    rejected: 'Rejected',
    acknowledged: 'Acknowledged',
    gone: 'Closed',
  }

  const OUTCOME: Record<string, string> = {
    not_attempted: 'publication not attempted',
    failed: 'publication failed',
    uncertain: 'publication outcome unknown',
  }
  const settled = $derived(
    publication?.latest && review && ['new', 'claimed', 'publishing'].includes(review.state)
      ? OUTCOME[publication.latest.outcome]
        ? publication.latest
        : null
      : null,
  )
</script>

{#if !loaded}
  <div class="empty">Loading review…</div>
{:else if !review && missing}
  <div class="banner crit">not found <b>· {error ?? 'no such review'}</b></div>
{:else if !review}
  <div class="banner crit" role="alert">
    Could not load this review <b>· {error}</b>
    <button class="lnk" onclick={() => attempt++}>Retry</button>
  </div>
{:else if report}
  <ReportReview {report} />
{:else}
  <div
    class="head"
    class:stale={!closed && stale.length > 0}
    class:approved={review.state === 'approved'}
    class:closed={closed && review.state !== 'approved'}
  >
    <span class="bar"></span>
    <span class="mono">{formatAge(review.created_ms, clock.now)}</span>
    <span class="t">
      <em>{closed ? CLOSED_LABEL[review.state] : stale.length > 0 ? 'Changed since submit' : 'Review'}</em>
      {title || review.title}
      <small
        >{files.length} files · +{review.added} −{review.removed} · {review.channel}{!closed && review.claimed_ms
          ? ' · claimed'
          : ''}</small
      >
    </span>
    {#if landed}
      <a class="pr" href={landed.url} target="_blank" rel="noreferrer">{publishedLabel(review.provider, landed, forgeState)}</a>
    {/if}
  </div>

  <dl class="kv">
    <dt>{review.state === 'approved' ? 'Published' : 'Publishes'}</dt>
    <dd class="m">
      {#if landed}
        <a href={landed.url} target="_blank" rel="noreferrer">{landed.number !== null ? `${noun} ${landed.number}` : landed.url}</a>
      {:else if change}
        updates <a href={change.url} target="_blank" rel="noreferrer">{noun} {change.number}</a>
      {:else if initial}
        the repository's first commit, no {noun}
      {:else}
        new {noun}{intent.forge.draft ? ' (draft)' : ''}
      {/if}
      {#if initial}
        → {target?.project} · {target?.branch}
      {:else}
        → {target?.project} · {outputs.branch ?? target?.branch} into {target?.base}
      {/if}
      {#if intent.rewrite}
        <span
          class="chip warn"
          title="the push replaces what the branch held at submit instead of adding to it"
          >rewrites history · replaces {intent.lease?.slice(0, 8)}</span
        >
      {/if}
    </dd>

    {#if review.session_id}
      <dt>Session</dt>
      <dd class="m"><a href="/sessions/{review.session_id}">{review.session_id.slice(0, 8)}</a></dd>
    {:else}
      <dt>Submitted by</dt>
      <dd class="m">{review.lane ?? 'an external agent'}</dd>
    {/if}
  </dl>
  {#if remoteOwner && ownerDetail?.state === 'fetched'}
    <div class="banner dim">
      held by {nodeLabel(store.nodes, remoteOwner)}
      <b>· read from it just now; it checks the branch for changes again when you decide</b>
    </div>
  {:else if remoteOwner}
    <div class="banner crit">
      held by {nodeLabel(store.nodes, remoteOwner)}
      <b>· {ownerDetail?.reason ?? 'its checks and evidence stay on that node'}; decide there, or reload once it can be read</b>
    </div>
  {/if}
  <!-- Before the evidence, prose and files: it is what saves reading them again. -->
  {#if sinceReviewed}
    <section class="since">
      <div class="h4">
        Since your last verdict
        <b>{sinceReviewed.head_sha.slice(0, 8)} → {review.head_sha.slice(0, 8)}</b>
      </div>
      {#each sinceReviewed.responses as r, i (i)}
        <div class="response">
          <span class="said">{r.source === 'operator' ? 'You' : r.source} · {r.decision === 'revise' ? 'asked' : r.decision}{r.sent_edit ? ' · with an edit' : ''}</span>
          <q>{r.reason || '(no reason given)'}</q>
          <span class="answer">
            {#if !r.answered_by}
              not answered yet
            {:else if r.files === null}
              answered · the files it changed can no longer be read
            {:else if r.files.length === 0}
              answered with no change to the files
            {:else}
              answered in {r.files.map((f) => f.path).join(', ')}
            {/if}
          </span>
        </div>
      {/each}
      {#if sinceReviewed.unavailable}
        <p class="note dim">{sinceReviewed.unavailable}</p>
      {/if}
    </section>
  {/if}
  <!-- The submitter's note and what it showed, before the evidence and files:
       its account of what it verified and what it did not is read first. -->
  <ShownWork items={shownWork} onReview reviewer={review.review_session_id} />
  {#if remoteOwner && !evidence}
    <!-- what the owner holds could not be read; the banner above says why -->
  {:else if !evidence}
    <div class="banner crit">
      verification evidence missing <b>· this review predates immutable candidate capture</b>
    </div>
  {:else}
    <section class="evidence">
      <div class="h4">Candidate evidence</div>
      <div class="evidence-grid">
        <div class="evidence-panel">
          <h3>Requirements</h3>
          {#if requirements}
            <b>{requirements.title}</b>
            {#if requirements.body.trim()}
              <div class="md">{@html renderMessage(requirements.body)}</div>
            {:else}
              <p>No additional requirement detail.</p>
            {/if}
            <a href="/tasks/{requirements.id}">open task</a>
          {:else}
            <p class="missing">No task was linked when this review was captured.</p>
          {/if}
        </div>
        <div class="evidence-panel">
          <h3>Runtime evidence</h3>
          {#if authoritativeChecks.length}
            {@const summary = runsSummary(authoritativeChecks)}
            <p class="runs-said" class:bad={authoritativeChecks.some((r) => runTone(r) === 'bad')}>{summary}</p>
            <ul class="runs">
              {#each authoritativeChecks as run (run.id)}
                {@const tone = runTone(run)}
                {@const failed = tone === 'bad' ? whatFailed(run) : ''}
                <li>
                  <div class="run">
                    <button class="line" type="button" aria-haspopup="dialog" onclick={() => (openRun = run)}>
                      <span class="dot {tone}" aria-label={outcomeLabel(run)}></span>
                      <code>{run.command ?? 'command not recorded'}</code>
                      <span class="dur">{tone === 'bad' ? `${outcomeLabel(run)} · ` : ''}{runDuration(run)}</span>
                    </button>
                    {#if failed}
                      <button
                        class="lnk fold"
                        type="button"
                        aria-expanded={!folded.has(run.id)}
                        aria-label={folded.has(run.id) ? 'show what failed' : 'hide what failed'}
                        onclick={() => fold(run.id)}>{folded.has(run.id) ? '▸' : '▾'}</button
                      >
                    {/if}
                  </div>
                  {#if failed && !folded.has(run.id)}
                    <pre class="failures" aria-label="what failed">{failed}</pre>
                  {/if}
                </li>
              {/each}
            </ul>
          {:else if checksRunning}
            <p class="missing">The node is running this candidate's required checks; they gate nothing{reviewerWaits ? ', and the reviewer starts once they are done' : ''}.</p>
          {:else if unrun}
            <p class="missing">{unrun.tail}</p>
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
              {#if c.finding}
                <small class="finding" class:contra={c.finding.outcome === 'contradicted'}>
                  review finding · {FINDING[c.finding.outcome]}{c.finding.note ? ` · ${c.finding.note}` : ''}
                  {#each c.finding.evidence as ev, n (ev)}
                    {' · '}<a href="#shown-{ev}">{shownTitle(ev) ?? `evidence ${n + 1}`}</a>
                  {/each}
                  · the reviewer's account, not a verdict
                </small>
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
              {:else if remoteOwner}
                <small>Judge it on {nodeLabel(store.nodes, remoteOwner)}, which holds this attempt.</small>
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
      {#each checks as c, index (index)}{@const running = c.outcome === 'running'}<span class="chip" class:ok={c.ok} class:bad={!c.ok && !running} title={c.ok ? undefined : c.tail}>{c.ok ? '✓' : running ? '…' : '✗'} {c.command}{running ? ' · running' : ` · ${Math.round(c.ms / 1000)}s`}</span>{/each}
    </div>
  {/if}

  <dl class="prov">
    <div><dt>Model</dt><dd>{session ? `${session.model} · ${session.phase}` : '—'}</dd></div>
    <div><dt>Task</dt><dd>{#if session?.work_item_id}<a href="/tasks/{session.work_item_id}">{session.work_item_id.slice(0, 8)}</a>{:else}none{/if}</dd></div>
    <div><dt>Reviewed by</dt><dd>{reviewer ? `${reviewer.model} · fresh session` : review.review_session_id ? 'fresh session' : (reviewerReason ?? 'no review session')}{reviewer && startedHow(reviewerView?.started_by) ? ` · ${startedHow(reviewerView?.started_by)}` : ''}</dd></div>
  </dl>

  {#if verdict}
    <div class="verdict" class:rc={verdict.verdict === 'request_changes'}>
      <span class="vbar"></span>
      <div class="in">
        <div class="who"><b>{verdict.verdict === 'approve' ? 'Approves' : 'Request changes'}</b>{verdictRevision !== null ? ` · for revision ${verdictRevision}` : ''} · {verdict.model}{basisLabel(verdict.basis) ? ` · ${basisLabel(verdict.basis)}` : ''}{reviewer && verdictRevision === null ? ` · ${Math.round(reviewer.tokens_used / 1000)}k tokens` : ''}</div>
        <div class="sum">{verdict.summary}</div>
        {#if verdict.findings?.length}
          <ul class="findings">
            {#each verdict.findings as f, i (i)}
              <li><span class="sev {f.severity ?? 'should'}">{f.severity ?? 'should'}</span><span class="path">{f.path ?? ''}{f.line ? `:${f.line}` : ''}</span><span class="fnote">{f.note}</span></li>
            {/each}
          </ul>
        {/if}
        {#if verdict.verdict === 'request_changes' && control?.kind === 'again' && !composing && !publishing}
          <div class="row"><button class="btn" disabled={busy} onclick={requestTheseChanges}>Request these changes…</button></div>
        {/if}
      </div>
    </div>
  {/if}

  {#if control?.kind === 'running'}
    <div class="banner dim">
      a fresh session is reading this review{control.since ? ` · ${formatAge(control.since, clock.now)}` : ''} <b>· its verdict lands here; yours decides</b>
    </div>
  {:else if remoteOwner && review.review_session_id && !verdict}
    <div class="banner dim">a fresh session is reading this review <b>· its verdict lands here; yours decides</b></div>
  {:else if control?.kind === 'no_model'}
    <div class="recover">
      <span class="note">No review model on {review.channel} · <a class="lnk" href="/settings#channels">Settings → Channels</a></span>
    </div>
  {:else if control?.kind === 'start'}
    <div class="recover">
      {#if control.note}<span class="note">{control.note}</span>{/if}
      <button class="btn" disabled={busy} onclick={startReviewer}>Start reviewer</button>
    </div>
  {:else if control?.kind === 'retry'}
    <div class="recover">
      <span class="note">{control.reason}</span>
      <button class="btn" disabled={busy} onclick={startReviewer}>Retry reviewer</button>
    </div>
  {:else if control?.kind === 'stale'}
    <div class="recover">
      <span class="note">the reviewer's verdict is on revision {control.revision ?? 'an earlier one'}; this is revision {reviewerView?.revision ?? 'a later one'}</span>
      <button class="btn" disabled={busy} onclick={startReviewer}>Review this revision</button>
    </div>
  {:else if control?.kind === 'again'}
    <div class="recover">
      {#if confirmingAgain}
        <span class="note">A new reviewer replaces the verdict shown.</span>
        <button class="btn" disabled={busy} onclick={() => { confirmingAgain = false; void startReviewer() }}>Review again</button>
        <button class="lnk" disabled={busy} onclick={() => (confirmingAgain = false)}>Keep this verdict</button>
      {:else}
        <button class="lnk" disabled={busy} onclick={() => (confirmingAgain = true)}>Review again…</button>
      {/if}
    </div>
  {/if}

  {#if review.state === 'revising'}
    <div class="banner ok">
      changes requested <b>· waiting on the agent to resubmit · {review.verdict_reason}</b>
    </div>
  {/if}
  {#if settled}
    <div class="banner crit">
      {OUTCOME[settled.outcome]}
      <b>· {settled.note ?? `${settled.provider} ${settled.project} · ${settled.branch}`}</b>
    </div>
    <div class="recover">
      {#if settled.recoverable}
        <span class="note">{settled.remedy}</span>
        <button class="btn p" disabled={busy} onclick={retry}>Retry publication</button>
      {:else if settled.refusal}
        <span class="note">{settled.refusal}</span>
      {/if}
      {#if !remoteOwner}
        {@render publishedAt()}
      {/if}
    </div>
  {:else if publishing}
    <div class="banner crit">
      publication outcome requires reconciliation <b>· this review may have reached the forge; do not approve, revise, or reject it again</b>
    </div>
    {#if publication?.latest && !remoteOwner}
      <div class="recover">{@render publishedAt()}</div>
    {/if}
  {/if}

  {#snippet publishedAt()}
    {#if reconcileChange === null}
      <button class="btn" disabled={busy} onclick={() => (reconcileChange = '')}>Published at…</button>
    {:else}
      <input
        class="edit change"
        placeholder="the pull or merge request you opened: its address or number"
        aria-label="The change you opened by hand"
        bind:value={reconcileChange}
        disabled={busy}
      />
      <button class="btn p" disabled={busy || !reconcileChange.trim()} onclick={reconcile}>Record it</button>
      <button class="btn" disabled={busy} onclick={() => (reconcileChange = null)}>Cancel</button>
      <span class="note dim">The node checks it on the forge: the pushed commit, this branch into the base, open or merged.</span>
    {/if}
  {/snippet}

  {#if authorship && authorship.misattributed.length > 0}
    <div class="banner">
      not authored as {authorship.identity.login}
      <b
        >· {authorship.misattributed.map((c) => `${c.sha.slice(0, 8)} by ${c.author_email}`).join(', ')} · ask the agent
        to re-author them; nothing is rewritten for you</b
      >
    </div>
  {/if}

  {#if !closed && stale.length > 0}
    <div class="banner crit">
      changed since submit <b>· {stale.join(', ')} · approve is disabled; ask the agent to resubmit</b>
    </div>
  {/if}

  {#if summaryOnly}
    <div class="h4">Summary <b>for you · not sent to the forge</b></div>
    <div class="summary">
      <b>{review.title}</b>
      <!-- The agent's Markdown, rendered as its session messages are: nothing fetched, links open beside. -->
      <div class="md">{@html renderMessage(review.body)}</div>
    </div>
  {:else}
    <div class="h4">
      Title and body <b>{closed ? 'as decided' : surface.phone ? 'edited on the desktop' : 'edit before approving if you want to'}</b>
    </div>
    <input class="edit" bind:value={title} disabled={busy || publishing || closed || surface.phone} />
    <textarea class="edit body grow" bind:value={body} use:autogrow={body} disabled={busy || publishing || closed || surface.phone}></textarea>
  {/if}

  <div class="h4">On the forge <b>{closed ? 'as decided' : remoteOwner ? `as the agent asked · edited on ${nodeLabel(store.nodes, remoteOwner)}` : surface.phone ? 'edited on the desktop' : 'what approval sends besides the commits'}</b></div>
  {#if remoteOwner}
    <div class="note dim">Approving publishes what the agent asked the forge to show; to change it, open this review on the node that holds it.</div>
  {:else}
  <label class="toggle">
    <input type="checkbox" bind:checked={describe} disabled={busy || publishing || closed || surface.phone} />
    {change ? `Replace the ${noun}'s title and description` : `Describe the ${noun} separately from the summary`}
  </label>
  {#if describe}
    <input class="edit" bind:value={descTitle} placeholder="title" disabled={busy || publishing || closed || surface.phone} />
    <textarea class="edit body grow" bind:value={descBody} use:autogrow={descBody} disabled={busy || publishing || closed || surface.phone}></textarea>
  {/if}
  <label class="toggle">
    <input type="checkbox" bind:checked={commenting} disabled={busy || publishing || closed || surface.phone} />
    Comment on the {noun}
  </label>
  {#if commenting}
    <textarea class="edit body grow" bind:value={comment} use:autogrow={comment} disabled={busy || publishing || closed || surface.phone}></textarea>
  {/if}
  {#if change && !describe && !(commenting && comment.trim())}
    <div class="note dim">Approving only pushes; the {noun}'s text is left as it is.</div>
  {/if}
  {/if}
  {#if edited && !closed}
    <div class="note">Edited. Approving publishes what is written here, not what was submitted.</div>
  {/if}

  {#if intent.commits?.length || squashing}
    <div class="h4">
      Commits
      <b
        >{squashing
          ? 'ships as one commit of the reviewed tree'
          : Object.keys(reworded).length
            ? `pushed with ${Object.keys(reworded).length === 1 ? 'one message' : `${Object.keys(reworded).length} messages`} edited here`
            : 'pushed as the agent wrote them'}</b
      >
    </div>
    {#if review && intent.commits?.length}
      {#if ships.own}
        <div class="note dim">
          Ships the agent's own commit <code>{review.head_sha.slice(0, 8)}</code
          >{(intent.signed ?? []).includes(review.head_sha) ? ', signature kept' : ''}.
        </div>
      {:else}
        <div class="note">
          Ships a new commit, not the agent's <code>{review.head_sha.slice(0, 8)}</code>{ships.dropped.length
            ? `. ${ships.dropped.length > 1 ? 'The signatures on' : 'The signature on'} ${ships.dropped.map((sha) => sha.slice(0, 8)).join(', ')} ${ships.dropped.length > 1 ? 'are' : 'is'} dropped: a commit the node makes is never signed`
            : ''}.{squashing && intent.as_written !== undefined ? " The agent's own message would ship its commit as written." : ''}
        </div>
      {/if}
    {/if}
    {#if intent.commits?.length}
      <ul class="commits">
        {#each intent.commits as c (c.sha)}
          <li class:squashed={squashing}>
            <div class="line">
              <span class="subject" title={c.subject}><code>{c.sha.slice(0, 8)}</code> {c.subject}</span>
              {#if reworded[c.sha] !== undefined}<span class="chip warn">edited</span>{/if}
              {#if rewordable && commitMessages[c.sha] === undefined}
                <button class="lnk" disabled={busy || publishing} onclick={() => editCommit(c)}>Edit message</button>
              {:else if rewordable}
                <button class="lnk" disabled={busy || publishing} onclick={() => revertCommit(c.sha)}
                  >{reworded[c.sha] !== undefined ? 'Undo' : 'Close'}</button
                >
              {/if}
            </div>
            {#if !squashing && commitMessages[c.sha] !== undefined}
              <CommitMessage
                label={`message of ${c.sha.slice(0, 8)}`}
                bind:value={commitMessages[c.sha]}
                disabled={busy || publishing || !rewordable}
              />
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
    {#if squashing && !remoteOwner}
      <CommitMessage label="commit message" bind:value={message} disabled={busy || publishing || closed || surface.phone} />
    {/if}
    {#if change === null && !initial && !remoteOwner}
      <label class="branch">
        Branch
        <input class="edit mono" bind:value={branchName} disabled={busy || publishing || closed || surface.phone} />
      </label>
    {/if}
  {/if}

  <div class="h4">Files <b>{files.length}</b></div>
  {#if !surface.phone}
    <div class="files">
      {#each files as f (f.path)}
        <button type="button" class:moved={stale.includes(f.path)} onclick={() => showFile(f.path)}>
          <span class="p">{f.path}</span>
          <span class={stale.includes(f.path) ? 'bad' : 'ok'}
            >{stale.includes(f.path) ? 'changed since submit' : 'unchanged'}</span
          >
        </button>
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
    {:else}
      {#if editable.size === 0}
        <div class="banner dim">
          nothing here can be edited <b>· each file below says why</b>
        </div>
      {/if}
      {#each [...editable] as [path, f] (path)}
        <div class="filehead" data-path={path}>
          <span class="p">{path}</span>
          {#if f.now !== f.head}<span class="chip warn">edited</span>{/if}
        </div>
        {#if DiffEditor}
          <DiffEditor {path} original={f.original} head={f.head} onchange={onFileChange} />
        {/if}
      {/each}
      {#each readOnly as r (r.path)}
        {@const block = diffFiles.find((d) => d.path === r.path)}
        <div class="filehead" data-path={r.path}>
          <span class="p">{r.path}</span>
          <span class="chip" class:warn={stale.includes(r.path)}>read-only · {r.why}</span>
        </div>
        {#if block}<Diff diff={block.lines.join('\n')} />{/if}
      {/each}
    {/if}
  {:else}
    {#if sinceReviewed?.diff}
      <div class="tabs" role="tablist">
        <button role="tab" class:on={diffView === 'since'} aria-selected={diffView === 'since'} onclick={() => (diffView = 'since')}>
          Since last verdict · {sinceReviewed.files?.length ?? 0} files
        </button>
        <button role="tab" class:on={diffView === 'full'} aria-selected={diffView === 'full'} onclick={() => (diffView = 'full')}>
          Full change · {files.length} files
        </button>
      </div>
    {/if}
    {#if diffView === 'since' && sinceReviewed?.diff}
      <Diff diff={sinceReviewed.diff} perFile={surface.phone} />
    {:else}
      <Diff diff={review.diff} perFile={surface.phone} />
    {/if}
  {/if}

  {#if closed}
    <!-- decided: there is nothing left to edit -->
  {:else if remoteOwner}
    <p class="note">Editing the diff needs the worktree, which is on {nodeLabel(store.nodes, remoteOwner)}.</p>
  {:else if !surface.phone}
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

  {#if readiness?.ready && readiness.note && !closed}
    <div class="note dim">{readiness.note}</div>
  {/if}

  {#if closed}
    <div class="decided" class:approved={review.state === 'approved'}>
      {#if review.state === 'approved'}
        {#if landed}
          approved and published as
          <a href={landed.url} target="_blank" rel="noreferrer">{landed.number !== null ? `${noun} ${landed.number}` : landed.url}</a>
        {:else}
          approved
        {/if}
      {:else if review.state === 'gone'}
        closed <b>· no longer open on the node that held it</b>
      {:else}
        {review.state}{#if review.verdict_reason}<b>{` · ${review.verdict_reason}`}</b>{/if}
      {/if}
    </div>
    {#if session?.work_item_id}
      <CloseTask itemId={session.work_item_id} />
    {/if}
  {:else}
  <!-- Everything that answers a verdict sits in the bar, so it is in view
       wherever on the page the verdict was given. -->
  <div class="decide" role="group" aria-label="Verdict">
    {#if error}
      <div class="banner crit">refused <b>· {error}</b></div>
    {/if}
    {#if draftState === 'conflict'}
      <div class="banner crit draft-conflict">
        <span>changed on another device <b>· {draftTheirs?.reason ? `it says “${draftTheirs.reason}”` : 'its draft differs from this one'}</b></span>
        <button class="btn" onclick={takeTheirs}>Use theirs</button>
        <button class="btn" onclick={keepMine}>Keep mine</button>
      </div>
    {/if}
    {#if draftMessage !== null}
      <div class="banner crit draft-conflict">
        <span>commit message drafted against an earlier revision <b>· it says “{draftMessage.split('\n')[0]}”; this revision's is in the box</b></span>
        <button class="btn" onclick={() => settleDraftMessage(true)}>Use the draft's</button>
        <button class="btn" onclick={() => settleDraftMessage(false)}>Keep this revision's</button>
      </div>
    {/if}
    {#if review && !ships.own && ships.dropped.length}
      <div class="banner">
        signature dropped <b>· approving ships a new, unsigned commit in place of the agent's signed {ships.dropped
            .map((sha) => sha.slice(0, 8))
            .join(', ')}</b>
        {#if squashing && intent.as_written !== undefined}<i>The agent's own message ships its commit as written, signature kept.</i>{/if}
      </div>
    {/if}
    {#if composing}
      {#if composing === 'revise' && aiFindings?.length}
        <fieldset class="ai-findings">
          <legend>From the AI review: untick a finding to leave it out</legend>
          {#each aiFindings as f, i (i)}
            <label>
              <input type="checkbox" checked={f.on} disabled={busy} onchange={(e) => tickFinding(i, e.currentTarget.checked)} />
              <span class="sev {f.finding.severity ?? 'should'}">{f.finding.severity ?? 'should'}</span>
              <span class="path">{f.finding.path ?? ''}{f.finding.line ? `:${f.finding.line}` : ''}</span>
              <span class="fnote">{f.finding.note}</span>
            </label>
          {/each}
        </fieldset>
      {/if}
      <label class="composer">
        <span>{composerLabel(composing, editedFiles)}</span>
        <textarea class="grow compact" bind:this={reasonInput} bind:value={reason} use:autogrow={reason} disabled={busy || publishing}></textarea>
      </label>
      <div class="row">
        <button
          class="btn {composing === 'reject' ? 'd' : 'p'}"
          disabled={sendBlocked !== null}
          onclick={() => composing && decide(composing)}
        >
          {composing === 'reject' ? 'Reject' : editedFiles > 0 ? 'Send edits and request changes' : 'Request changes'}
        </button>
        <button class="btn" disabled={busy} onclick={closeComposer}>Back</button>
        {#if sendBlocked}<span class="why">{sendBlocked}</span>{/if}
      </div>
    {:else}
      <div class="row verdicts">
        <button class="btn p" disabled={approveBlocked !== null} onclick={() => decide('approve')}>
          Approve and publish
        </button>
        <button class="btn" disabled={busy || publishing} onclick={() => compose('revise')}>
          {editedFiles > 0 ? 'Send edits and request changes…' : 'Request changes…'}
        </button>
        <button class="btn d" disabled={busy || publishing} onclick={() => compose('reject')}>Reject…</button>
      </div>
      {#if approveBlocked}
        <p class="why">
          Approve: {approveBlocked}.
          {#if readiness && !readiness.ready && !busy && !publishing && stale.length === 0}
            <a class="lnk" href={readiness.settings}>Bind a token in Settings</a>
          {/if}
        </p>
      {/if}
    {/if}
    <div class="draft-state" aria-live="polite">{draftLine}</div>
  </div>
  {/if}
  <CheckDetail run={openRun} onclose={() => (openRun = null)} />
{/if}

<style>
  .recover {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
    margin: 6px 0 12px;
  }
  .recover .change {
    flex: 1 1 260px;
    min-width: 0;
  }
  .head {
    display: grid;
    grid-template-columns: 3px 72px minmax(0, 1fr) auto;
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
  .head.approved {
    background: linear-gradient(90deg, var(--wash-ok), var(--s1) 42%);
  }
  .head.approved .bar {
    background: var(--ok);
  }
  .head.closed {
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 42%);
  }
  .head.closed .bar {
    background: var(--dim);
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
  .head.approved .t em {
    color: var(--ok);
  }
  .head.closed .t em {
    color: var(--dim);
  }
  /* The published change, where the review now lives on. */
  .head .pr {
    font: 12.5px var(--mono);
    white-space: nowrap;
    color: var(--acc);
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
    grid-template-columns: repeat(2, minmax(0, 1fr));
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
  .evidence-panel .md {
    margin: 5px 0;
    overflow-wrap: anywhere;
  }
  /* Wrapped: a failure line cut off at the panel's edge says nothing. */
  .evidence pre.failures {
    max-height: 210px;
    margin: 2px 0 4px 16px;
    overflow: auto;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    padding-left: 8px;
    border-left: 2px solid var(--crit);
    font: 11px/1.45 var(--mono);
    color: var(--ink2);
  }
  .runs-said {
    font: 11.5px var(--mono);
    color: var(--ok);
  }
  .runs-said.bad {
    color: var(--crit);
  }
  .runs {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .runs li {
    border-top: 1px solid var(--rule);
  }
  .runs li:first-child {
    border-top: 0;
  }
  .run {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  /* One line per check: the whole of it opens the run's detail. */
  .run .line {
    flex: 1;
    min-width: 0;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 0;
    background: none;
    border: 0;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }
  .run .line:hover code {
    color: var(--ink);
  }
  .run code {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font: 11.5px var(--mono);
    color: var(--ink2);
  }
  .run .dur {
    font: 11px var(--mono);
    color: var(--dim);
    white-space: nowrap;
  }
  .run .fold {
    font-size: 11px;
    color: var(--dim);
  }
  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--wait);
  }
  .dot.ok {
    background: var(--ok);
  }
  .dot.bad {
    background: var(--crit);
  }
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
  .checks .chip.bad {
    background: var(--wash-crit);
    color: var(--crit);
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
  .criteria-block small.finding {
    color: var(--ink2);
  }
  .criteria-block small.finding.contra {
    color: var(--crit);
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
    overflow-wrap: anywhere;
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
  .ai-findings {
    display: flex;
    flex-direction: column;
    gap: 4px;
    border: 0;
    margin: 0 0 8px;
    padding: 0;
    min-width: 0;
  }
  .ai-findings legend {
    font-size: 12px;
    color: var(--dim);
    padding: 0;
    margin-bottom: 4px;
  }
  .ai-findings label {
    display: grid;
    grid-template-columns: 18px 64px 170px minmax(0, 1fr);
    gap: 10px;
    align-items: baseline;
    font-size: 12.5px;
  }
  @media (max-width: 700px) {
    .evidence-grid {
      grid-template-columns: 1fr;
    }
    .findings li {
      grid-template-columns: 64px 1fr;
    }
    .ai-findings label {
      grid-template-columns: 18px 64px minmax(0, 1fr);
    }
    .ai-findings .fnote {
      grid-column: 2 / -1;
    }
    .path {
      grid-column: 2;
    }
    /* Under its severity and path, not squeezed into the severity's column. */
    .fnote {
      grid-column: 1 / -1;
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
  .summary .md {
    margin: 5px 0 0;
    color: var(--ink2);
    overflow-wrap: anywhere;
  }
  /* Rendered Markdown: the summary and the pinned requirements. */
  .md > :global(:first-child) {
    margin-top: 0;
  }
  .md > :global(:last-child) {
    margin-bottom: 0;
  }
  .md :global(p),
  .md :global(ul),
  .md :global(ol),
  .md :global(pre),
  .md :global(blockquote) {
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
    margin: 0.7em 0 0.2em;
    font-size: 1em;
    font-weight: 600;
    color: var(--ink);
  }
  .md :global(code) {
    font: 0.9em var(--mono);
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
  .md :global(blockquote) {
    border-left: 2px solid var(--rule);
    padding-left: 1.5ch;
  }
  .md :global(a) {
    color: var(--acc);
  }
  .md :global(.no-fetch) {
    color: var(--dim);
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
  .commits {
    list-style: none;
    margin: 0 0 8px;
    padding: 0;
    font: 12.5px var(--mono);
  }
  .commits li {
    padding: 3px 0;
  }
  .commits .line {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }
  .commits .subject {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .commits .chip.warn {
    background: var(--wash-wait);
    color: var(--wait);
  }
  .commits li.squashed {
    color: var(--dim);
  }
  .commits code {
    color: var(--dim);
    margin-right: 6px;
  }
  .branch {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 6px 0 12px;
    font-size: 12.5px;
    color: var(--dim);
  }
  .branch input {
    flex: 1;
    margin: 0;
  }
  .files {
    background: var(--s1);
    border-radius: 4px;
    font: 12.5px var(--mono);
    overflow: hidden;
  }
  /* Each file is a way into its changes in the diff below. */
  .files button {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    width: 100%;
    padding: 6px 12px;
    border: 0;
    border-top: 1px solid var(--rule);
    background: none;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .files button:first-child {
    border-top: 0;
  }
  .files button:hover .p {
    color: var(--acc);
  }
  .files .p {
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .files .ok {
    color: var(--dim);
  }
  .files .bad {
    color: var(--crit);
  }
  .draft-state {
    font: 12px var(--mono);
    color: var(--dim);
  }
  /* Kept in the page for the live region; takes no room while it says nothing. */
  .draft-state:empty {
    margin-top: -8px;
  }
  .draft-conflict {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .since {
    margin: 12px 0;
  }
  .since .response {
    display: grid;
    gap: 2px;
    padding: 6px 0;
    border-top: 1px solid var(--rule);
    font-size: 13px;
  }
  .since .said,
  .since .answer {
    color: var(--dim);
    font: 12px var(--mono);
  }
  .since q {
    quotes: none;
  }
  .tabs {
    display: flex;
    gap: 6px;
    margin: 10px 0 6px;
  }
  .tabs button {
    font: 12px var(--mono);
    padding: 4px 10px;
    border: 1px solid var(--rule);
    border-radius: 4px;
    background: var(--s1);
    color: var(--dim);
  }
  .tabs button.on {
    color: inherit;
    border-color: currentColor;
  }
  /* However long the review, the verdict stays on screen, above the phone's
     bottom tabs. */
  .decide {
    position: sticky;
    bottom: var(--tabbar);
    z-index: 5;
    display: grid;
    gap: 8px;
    background: var(--bg);
    border-top: 1px solid var(--rule);
    padding: 12px 0 calc(12px + env(safe-area-inset-bottom));
  }
  .decide .row {
    display: flex;
    gap: 10px;
    flex-wrap: wrap;
    align-items: center;
  }
  .decide .composer {
    display: grid;
    gap: 6px;
    font-size: 12.5px;
    color: var(--dim);
  }
  .decide textarea {
    background: var(--s1);
    border: 0;
    border-radius: 4px;
    color: var(--ink);
    padding: 8px 10px;
    font: 13px var(--sans);
  }
  .decide .why {
    margin: 0;
    font-size: 12px;
    color: var(--dim);
  }
  .decided {
    border-left: 3px solid var(--dim);
    border-radius: 4px;
    background: linear-gradient(90deg, var(--wash-dim), var(--s1) 50%);
    padding: 10px 12px;
    font: 12.5px var(--mono);
    color: var(--ink2);
    overflow-wrap: anywhere;
  }
  .decided b {
    font-weight: 400;
    color: var(--ink);
  }
  .decided.approved {
    border-left-color: var(--ok);
    background: linear-gradient(90deg, var(--wash-ok), var(--s1) 50%);
    color: var(--ok);
  }
  @media (max-width: 700px) {
    .decide {
      padding-bottom: 12px;
    }
    /* Three verdicts in one row: two rows of buttons would take a quarter of
       the screen above the tabs. */
    .decide .row.verdicts {
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      gap: 8px;
    }
    .decide .row.verdicts .btn {
      padding: 6px 8px;
      line-height: 1.25;
    }
  }
</style>
