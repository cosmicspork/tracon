// What the continuation panel says about each attempt and how they connect.
// Pure, so the wording is tested once rather than read off a screen.

import type { Continuation, ContinuationAttempt } from './types'

const ENDED: Record<string, string> = {
  killed_user: 'stopped',
  budget: 'spent its budget',
  harness_exit: 'harness exited',
  item_close: 'item closed',
  phase_done: 'phase done',
  detached: 'detached',
  incompatible: 'harness incompatible',
  node_restart: 'node restarted',
  provider_exhausted: 'provider exhausted',
  error: 'failed',
}

/** How one attempt stands, in a few words. */
export function attemptState(a: ContinuationAttempt): string {
  if (a.end_reason) {
    const why = ENDED[a.end_reason] ?? a.end_reason.replaceAll('_', ' ')
    return a.end_reason === 'error' && a.last_error ? `${why}: ${a.last_error}` : why
  }
  return a.state.replaceAll('_', ' ')
}

/** Which earlier attempt this one carries on, as a short id, or null for a first try. */
export function carries(a: ContinuationAttempt, all: ContinuationAttempt[]): string | null {
  const from = a.continued_from ?? a.parent_session
  if (!from) return null
  return all.some((b) => b.id === from) ? from.slice(0, 8) : null
}

/** The verbs the panel offers, by what the node says is possible. */
export function verbs(c: Continuation): { continue: boolean; changeApproach: boolean; abandon: boolean } {
  const from = c.actions.continue_from !== null
  return {
    // A plain continuation is offered where the work was interrupted rather
    // than where it went wrong; the other way round, the change comes first.
    continue: from && c.next.kind !== 'change_approach',
    changeApproach: from,
    abandon: c.actions.abandon,
  }
}
