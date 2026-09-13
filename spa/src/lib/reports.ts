import { call } from './api'
import type { Review } from './types'

export type ReportState = 'new' | 'claimed' | 'revising' | 'acknowledged'
export type NarrativeReport = Omit<Review, 'kind' | 'state'> & {
  kind: 'report'
  state: ReportState
}

/** A report is queue-only: it has no Git candidate or publication path. */
export function isNarrativeReport(review: Review): review is NarrativeReport {
  return review.kind === 'report'
}

export function decideReport(
  id: string,
  verdict: {
    verdict: 'acknowledge' | 'request_changes'
    reason?: string
    /** Hash of the narrative version displayed to the operator. */
    head_sha: string
  },
) {
  return call<{ state: ReportState }>('POST', `/api/reviews/${id}/verdict`, verdict)
}
