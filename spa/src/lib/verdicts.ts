// Which verdicts a review screen can send right now, and in words why not.
// A disabled button with no reason is a puzzle; every action that cannot be
// taken says so beside it instead.

import type { ReviewerView } from './types'

export type VerdictAction = 'approve' | 'revise' | 'reject'

export interface VerdictConditions {
  /** A verdict is on its way. */
  busy: boolean
  /** A publication may have reached the forge and needs reconciling first. */
  publishing: boolean
  /** Files changed in the worktree since submit. */
  stale: string[]
  /** What the reason composer holds, for the verdicts that need one. */
  reason: string
  /** Whether this node can publish an approval; absent when it is not asked. */
  readiness?: { ready: boolean; problem?: string | null } | null
}

/** Why `action` cannot be sent, or null when it can. */
export function unavailable(action: VerdictAction, c: VerdictConditions): string | null {
  if (c.busy) return 'sending your verdict'
  if (c.publishing) {
    return 'this review may already be on the forge; reconcile the publication before deciding again'
  }
  if (action === 'approve') {
    if (c.stale.length > 0) {
      const files = c.stale.length === 1 ? c.stale[0] : `${c.stale.length} files`
      return `${files} changed since submit; ask the agent to resubmit, then approve what it sends`
    }
    if (c.readiness && !c.readiness.ready) {
      return `cannot publish from this node yet: ${c.readiness.problem ?? 'its forge token is not ready'}`
    }
    return null
  }
  if (!c.reason.trim()) {
    return action === 'revise' ? 'say what should change' : 'say why you are rejecting it'
  }
  return null
}

/**
 * A review's checks record in a word: the node is still running them (an
 * external candidate's, which gate nothing), they all passed, or not.
 */
export function checksState(checks: { ok: boolean; outcome?: string }[]): 'running' | 'passed' | 'failed' | null {
  if (!checks.length) return null
  if (checks.some((c) => c.outcome === 'running')) return 'running'
  return checks.every((c) => c.ok) ? 'passed' : 'failed'
}

/** The label of the composer that opens for a verdict needing a reason. */
export function composerLabel(action: Exclude<VerdictAction, 'approve'>, editedFiles: number): string {
  if (action === 'reject') return 'Why are you rejecting it? The agent reads this.'
  return editedFiles > 0
    ? `What should change, beyond your edits to ${editedFiles} file${editedFiles === 1 ? '' : 's'}? The agent reads this.`
    : 'What should change? The agent reads this.'
}

/** States a review is finished in: no verdict can be sent on it any more. */
export const CLOSED_STATES = ['approved', 'rejected', 'acknowledged', 'gone'] as const

/**
 * The change an approval opened or updated, from the URL the node recorded
 * as its publication result. The number is read from the forge's own path,
 * and is null when the URL does not carry one.
 */
export function publishedChange(result: string | null | undefined): { url: string; number: number | null } | null {
  const url = result?.trim()
  if (!url || !/^https?:\/\//.test(url)) return null
  const m = /\/(?:pull|pulls|merge_requests)\/(\d+)(?:[/?#]|$)/.exec(url)
  return { url, number: m ? Number(m[1]) : null }
}

/** What the review page offers of its reviewer, from what happened to the latest revision. */
export type ReviewerControl =
  /** Decided, or another node's review: nothing to start here. */
  | { kind: 'hidden' }
  /** The channel binds no review model, so no reviewer can start. */
  | { kind: 'no_model' }
  /** None has run on this revision: "Start reviewer". `note` says why none did. */
  | { kind: 'start'; note: string | null }
  /** One is starting or reading it: no button. */
  | { kind: 'running'; since: number | null }
  /** Its verdict on this revision is in: "Review again", which replaces it. */
  | { kind: 'again' }
  /** It ended without a verdict: the reason, and "Retry reviewer". */
  | { kind: 'retry'; reason: string }
  /** The verdict shown is on an earlier revision: "Review this revision". */
  | { kind: 'stale'; revision: number | null }

/**
 * The reviewer control for a review in `state`. `reviewer` is the node's
 * account of the latest revision's reviewer (absent for a mirrored review);
 * `starting` is the node saying one is being started that it has not
 * recorded yet.
 */
export function reviewerControl(
  state: string,
  reviewer: ReviewerView | null | undefined,
  starting = false,
): ReviewerControl {
  if (!['new', 'claimed', 'revising'].includes(state) || !reviewer) return { kind: 'hidden' }
  if (!reviewer.bound) return { kind: 'no_model' }
  if (starting || reviewer.state === 'starting' || reviewer.state === 'running') {
    return { kind: 'running', since: reviewer.started_ms ?? null }
  }
  if (reviewer.state === 'done') return { kind: 'again' }
  if (reviewer.state === 'failed') return { kind: 'retry', reason: reviewer.reason ?? 'the reviewer ended without a verdict' }
  if (reviewer.verdict_revision != null) return { kind: 'stale', revision: reviewer.verdict_revision }
  return { kind: 'start', note: reviewer.reason }
}

/**
 * When the review page reads its reviewer again while one is starting or
 * running: `now` once the very session the node said was reading it has
 * ended, so its verdict or failure shows at once; otherwise `later`, after
 * a pause. A session that ended before this one was recorded (the last
 * reviewer, while a new one starts) is not a reason to read again at once:
 * the read would find the same, and ask again, as fast as it could.
 */
export function rereadReviewer(
  control: ReviewerControl | null,
  reviewer: ReviewerView | null | undefined,
  starting: boolean,
  live: { id: string; state: string } | null | undefined,
): 'now' | 'later' | null {
  if (control?.kind !== 'running') return null
  const tracking = !starting && (reviewer?.state === 'running' || reviewer?.state === 'starting')
  const ended = live && ['closed', 'killed_budget', 'failed'].includes(live.state)
  return tracking && ended && live.id === reviewer?.session_id ? 'now' : 'later'
}

/**
 * How long to wait before reading the reviewer again, in ms, after
 * `failures` reads in a row that failed: at once only when nothing has
 * failed, then backing off to at most half a minute.
 */
export function rereadDelay(when: 'now' | 'later', failures: number): number {
  if (when === 'now' && failures === 0) return 0
  return Math.min(3000 * 2 ** Math.max(0, failures - 1), 30000)
}

/** How the reviewer came to start, for the "Reviewed by" line. */
export function startedHow(startedBy: string | null | undefined): string | null {
  if (startedBy === 'submission') return 'started on submission'
  if (startedBy === 'operator') return 'started by you'
  return null
}

/** What a reviewer's verdict can rest on, as `review_verdict` takes it. */
export type VerdictBasis = 'ran_checks' | 'cited_evidence' | 'diff_only'

const BASIS: Record<VerdictBasis, string> = {
  ran_checks: 'ran the checks',
  cited_evidence: 'cited evidence',
  diff_only: 'diff only',
}

/**
 * What a reviewer's verdict rests on, in the words the card shows beside its
 * model. Null for a verdict given before reviewers said, or a basis this
 * screen does not know: nothing is said rather than a guess.
 */
export function basisLabel(basis: string | null | undefined): string | null {
  return basis && basis in BASIS ? BASIS[basis as VerdictBasis] : null
}
