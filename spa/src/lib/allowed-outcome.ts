// What became of a call the operator allowed.
//
// The node accepts an Allow and runs the call afterwards, so the answer
// returning says only that it was decided. How the run ended (succeeded,
// failed, failed as stale, uncertain) arrives later, as the approval's
// `approval_settled` event on the stream. The approval page waits for it a
// while; past that, or once the operator has left, the shell raises a toast
// for a call that did not run, and the home feed lists it until opened.

import { STALE_REASON } from './approval'

/** How an allowed call ended, as the stream (or the queue) said. */
export interface AllowedOutcome {
  id: string
  tool: string
  /** The card's title, when known: the queue carries it, the stream does not. */
  title?: string | null
  /** `succeeded`, `failed` or `uncertain`. */
  state: string
  reason: string | null
  stale: boolean
  session_id?: string | null
  at_ms: number
}

/** What the page says of a stale run, in the operator's terms. */
export const STALE_OUTCOME = 'The rows changed since you looked; nothing was sent. The agent has to ask again.'

/** What the page says when Allow itself was refused as stale and the card reloaded. */
export const STALE_ALLOW = 'This changed since you looked; review it again.'

/** How long the page follows an allowed call before leaving it to the toast. */
export const OUTCOME_WAIT_MS = 15_000

/** How long "Ran" shows before the page leaves. */
export const RAN_MS = 1_200

/** How long a toast stays unless dismissed or opened. */
export const TOAST_MS = 30_000

/** An outcome older than this when it reaches the shell (a replay, a peer's
    backfill) is the home feed's to show, not a toast's. */
export const TOAST_RECENT_MS = 10 * 60_000

const ENDS = new Set(['succeeded', 'failed', 'uncertain'])

/** Whether a failed run was refused as stale: nothing was sent. */
export function isStale(state: string, reason: string | null | undefined): boolean {
  return state === 'failed' && !!reason?.startsWith(STALE_REASON)
}

/** An `approval_settled` event read as an allowed call's outcome, or null for any other event. */
export function settledFrom(
  kind: string,
  refId: string | null | undefined,
  payload: Record<string, unknown> | null | undefined,
  atMs: number,
  sessionId?: string | null,
): AllowedOutcome | null {
  if (kind !== 'approval_settled' || !payload) return null
  const state = typeof payload.state === 'string' ? payload.state : ''
  if (!ENDS.has(state)) return null
  const id = typeof payload.approval_id === 'string' ? payload.approval_id : (refId ?? '')
  if (!id) return null
  const reason = typeof payload.reason === 'string' ? payload.reason : null
  return {
    id,
    tool: typeof payload.tool === 'string' ? payload.tool : '',
    state,
    reason,
    stale: isStale(state, reason),
    session_id: sessionId ?? null,
    at_ms: atMs,
  }
}

/** Whether the call did not run, or may not have: what the operator must be told. */
export function didNotRun(o: Pick<AllowedOutcome, 'state'>): boolean {
  return o.state === 'failed' || o.state === 'uncertain'
}

/** The outcome of approval `id` that settled at or after `sinceMs`, newest first in `list`. */
export function outcomeOf(list: readonly AllowedOutcome[], id: string, sinceMs = 0): AllowedOutcome | undefined {
  return list.find((o) => o.id === id && o.at_ms >= sinceMs)
}

/** A stale reason without its marker, or the reason as it is. */
function why(reason: string | null): string {
  const r = (reason ?? '').trim()
  return r.startsWith(STALE_REASON) ? r.slice(STALE_REASON.length).trim() : r
}

/** One line for a call that did not run: a toast, or a home feed row. */
export function notRunLine(o: AllowedOutcome): string {
  const name = o.title?.trim() || o.tool || 'the call'
  if (o.stale) return `${name} didn't run: the rows changed since you looked`
  if (o.state === 'uncertain') return `${name} may not have run: its outcome could not be confirmed`
  const r = why(o.reason)
  return r ? `${name} didn't run: ${r}` : `${name} didn't run`
}

/** What the approval page says under its outcome for a call that did not run. */
export function notRunDetail(state: string, reason: string | null | undefined): string | null {
  if (isStale(state, reason)) return STALE_OUTCOME
  if (state === 'failed') return 'It did not run. What it was refused with is below.'
  if (state === 'uncertain') return 'It was sent, but whether it took could not be confirmed: check before asking again.'
  return null
}

/** Where an accepted Allow has got to on the page that sent it. */
export type AfterAllow =
  | { kind: 'running' }
  | { kind: 'ran' }
  | { kind: 'stay'; outcome: AllowedOutcome }
  | { kind: 'leave' }

/**
 * What the page does `waitedMs` after its Allow was accepted, given the
 * outcome seen so far: keep following, show "Ran", stay on a call that did
 * not run, or leave it to the toast once it has waited long enough.
 */
export function afterAllow(outcome: AllowedOutcome | undefined, waitedMs: number): AfterAllow {
  if (outcome?.state === 'succeeded') return { kind: 'ran' }
  if (outcome && didNotRun(outcome)) return { kind: 'stay', outcome }
  return waitedMs < OUTCOME_WAIT_MS ? { kind: 'running' } : { kind: 'leave' }
}

/** The approval page for `id` is the one on screen. */
export function onApprovalPage(path: string, id: string): boolean {
  return path === `/approvals/${id}` || path.startsWith(`/approvals/${id}/`)
}

/** A toast the shell shows. */
export interface OutcomeToast {
  id: string
  text: string
  href: string
}

/**
 * Decides which outcomes raise a toast: an allowed call that did not run,
 * lately, once, unless its own page is open (it says so there) or it was
 * opened.
 */
export class ToastGate {
  private noticed = new Set<string>()

  notice(o: AllowedOutcome, path: string, seen: ReadonlySet<string>, now: number): OutcomeToast | null {
    if (!didNotRun(o) || this.noticed.has(o.id) || seen.has(o.id)) return null
    this.noticed.add(o.id)
    if (now - o.at_ms > TOAST_RECENT_MS) return null
    if (onApprovalPage(path, o.id)) return null
    return { id: o.id, text: notRunLine(o), href: `/approvals/${o.id}` }
  }
}

const SEEN_KEY = 'tracon-seen-outcomes'
const SEEN_MAX = 200

/** The not-run outcomes this browser has opened. Blocked storage reads as none. */
export function loadSeen(): Set<string> {
  try {
    const raw = localStorage.getItem(SEEN_KEY)
    const ids = raw ? JSON.parse(raw) : []
    return new Set(Array.isArray(ids) ? ids.filter((i): i is string => typeof i === 'string') : [])
  } catch {
    return new Set()
  }
}

/** Remember that `seen` were opened, keeping the most recent. */
export function saveSeen(seen: ReadonlySet<string>): void {
  try {
    localStorage.setItem(SEEN_KEY, JSON.stringify([...seen].slice(-SEEN_MAX)))
  } catch {
    /* blocked storage: the feed keeps them until the window closes */
  }
}

/** How many not-run calls the home feed lists. */
export const NOT_RUN_SHOWN = 6

/** The feed's not-run calls: the queue's and the stream's, once each, newest first, less those opened, at most `limit`. */
export function feed(
  queued: readonly AllowedOutcome[],
  streamed: readonly AllowedOutcome[],
  seen: ReadonlySet<string>,
  limit = NOT_RUN_SHOWN,
): AllowedOutcome[] {
  const byId = new Map<string, AllowedOutcome>()
  for (const o of [...streamed, ...queued]) {
    if (!didNotRun(o) || seen.has(o.id)) continue
    const had = byId.get(o.id)
    byId.set(o.id, had ? { ...had, title: had.title ?? o.title, session_id: had.session_id ?? o.session_id } : o)
  }
  return [...byId.values()].sort((a, b) => b.at_ms - a.at_ms).slice(0, limit)
}
