// The review history (/reviews): decided reviews, newest first, a page at a
// time, from the node's own and the ones it mirrors. What the screen asks the
// node for and how it names each row.

import { nameOf } from './names'
import type { DecidedReview } from './types'

/** What the history is narrowed to. */
export interface HistoryFilter {
  /** Only reviews from a harness the operator runs. */
  external: boolean
  /** One channel, or '' for all. */
  channel: string
}

/** The query for one page: the first, or the one after `before` (a page's `next`). */
export function historyQuery(filter: HistoryFilter, before: string | null = null): string {
  const q = new URLSearchParams({ state: 'decided' })
  if (filter.external) q.set('source', 'external')
  if (filter.channel) q.set('channel', filter.channel)
  if (before) q.set('before', before)
  return q.toString()
}

/** The verdict, as the operator gave it. */
export function verdictLabel(state: DecidedReview['state']): string {
  return state === 'approved' ? 'Approved' : 'Rejected'
}

/** Where the work is: the forge project, else the provider. */
export function repoLabel(r: DecidedReview): string {
  return r.project || r.provider
}

/**
 * Where a review came from: the session that submitted it, by name, or a
 * harness the operator runs, by the lane it gave, on its channel.
 */
export function sourceLabel(r: DecidedReview): string {
  if (r.session_id) return nameOf('session', r.session_id)
  return ['your own harness', r.lane, r.channel].filter(Boolean).join(' · ')
}

/** Rows from a later page added after those already shown, each review once. */
export function appendPage(shown: DecidedReview[], page: DecidedReview[]): DecidedReview[] {
  const seen = new Set(shown.map((r) => r.id))
  return [...shown, ...page.filter((r) => !seen.has(r.id))]
}
