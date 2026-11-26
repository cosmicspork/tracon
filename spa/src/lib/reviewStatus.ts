// The review page's status line: who checked this, in one line under the
// title. Each item says only what it has to say, and is left out otherwise.

import { runTone } from './checkRuns'
import type { AiVerdict, CandidateCheckRun, CheckResult } from './types'
import type { ReviewerControl } from './verdicts'

/** The id chip's text: the first 8 hex digits, as agents write it in notes. */
export function shortId(id: string): string {
  const idHex = id.replace(/-/g, '')
  return idHex.slice(0, 8)
}

/** The checks item: its words, how it reads, and whether it opens to the runs. */
export interface ChecksStatus {
  label: string
  tone: 'ok' | 'bad' | 'running' | 'none'
  /** It opens to the runs list (or what the node said of them). */
  expands: boolean
}

/**
 * The checks item, from the node's runs on the candidate or, without any,
 * the review's own older record of them. `reviewerWaits`: a reviewer starts
 * once running checks end.
 */
export function checksStatus(
  runs: CandidateCheckRun[],
  recorded: CheckResult[],
  reviewerWaits = false,
): ChecksStatus {
  const running = (label: string): ChecksStatus => ({
    label: `${label}${reviewerWaits ? ' · reviewer starts after' : ''}`,
    tone: 'running',
    expands: true,
  })
  if (runs.length) {
    const bad = runs.filter((r) => runTone(r) === 'bad').length
    if (bad) return { label: `checks ✗ ${bad} of ${runs.length}`, tone: 'bad', expands: true }
    if (runs.some((r) => runTone(r) === 'running')) return running('checks running')
    return { label: `checks ✓ ${runs.length}`, tone: 'ok', expands: true }
  }
  if (recorded.length) {
    if (recorded.some((c) => c.outcome === 'could_not_run')) {
      return { label: 'checks could not run', tone: 'bad', expands: true }
    }
    if (recorded.some((c) => c.outcome === 'running')) return running('checks running')
    const bad = recorded.filter((c) => !c.ok).length
    if (bad) return { label: `checks ✗ ${bad} of ${recorded.length}`, tone: 'bad', expands: true }
    return { label: `checks ✓ ${recorded.length}`, tone: 'ok', expands: true }
  }
  return { label: 'no checks', tone: 'none', expands: false }
}

/** The reviewer item: a verdict, a reviewer at work, or the control that starts one. */
export type ReviewerStatus =
  /** One is starting or reading this revision. */
  | { kind: 'reviewing'; since: number | null }
  /** A peer's review whose owner can't be read: why no control shows. */
  | { kind: 'away'; note: string }
  /** The channel binds no review model: a link to Settings → Channels. */
  | { kind: 'no_model' }
  /**
   * Its verdict, and the action beside it: `again` replaces it, `stale` reviews
   * this revision when the verdict is on `revision`, an earlier one.
   */
  | { kind: 'verdict'; label: 'approves' | 'requests changes'; action: 'again' | 'stale' | null; revision: number | null }
  /** None has run on this revision: "Start reviewer", with why none did. */
  | { kind: 'start'; note: string | null }
  /** It ended without a verdict: "Retry reviewer", with why. */
  | { kind: 'retry'; reason: string }
  /** The verdict is on an earlier revision and could not be read: "Review this revision". */
  | { kind: 'stale'; revision: number | null }

/**
 * The reviewer item, from the control the page offers and the verdict on the
 * card. `away` is why a peer's review offers no control; `peerReading` is a
 * peer's reviewer reading it that its owner could not be asked about. Null when
 * there is nothing to say: a decided review no reviewer read.
 */
export function reviewerStatus(
  control: ReviewerControl | null,
  verdict: Pick<AiVerdict, 'verdict'> | null,
  opts: { away?: string | null; peerReading?: boolean } = {},
): ReviewerStatus | null {
  const label = verdict ? (verdict.verdict === 'approve' ? 'approves' : 'requests changes') : null
  if (control?.kind === 'running') return { kind: 'reviewing', since: control.since }
  if (opts.peerReading && !verdict) return { kind: 'reviewing', since: null }
  if (opts.away) return { kind: 'away', note: opts.away }
  switch (control?.kind) {
    case 'no_model':
      return { kind: 'no_model' }
    case 'start':
      return { kind: 'start', note: control.note }
    case 'retry':
      return { kind: 'retry', reason: control.reason }
    case 'stale':
      return label
        ? { kind: 'verdict', label, action: 'stale', revision: control.revision }
        : { kind: 'stale', revision: control.revision }
    case 'again':
      // Done with no verdict to show: starting another replaces nothing.
      return label ? { kind: 'verdict', label, action: 'again', revision: null } : { kind: 'start', note: null }
  }
  return label ? { kind: 'verdict', label, action: null, revision: null } : null
}
