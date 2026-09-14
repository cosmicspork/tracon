import { call } from './api'

export interface EvidenceCursor {
  captured_ms: number
  id: string
}

export interface EvidenceWorkLink {
  id: string
  title: string
  state: 'open' | 'closed'
}

export interface EvidenceReviewLink {
  id: string
  title: string
  state: string
}

export interface EvidenceCandidate {
  candidate: {
    id: string
    head_sha: string
    channel: string
    owner_session_id: string
    owner_node_id: string | null
    source_kind: string
    captured_ms: number
  }
  work_items: EvidenceWorkLink[]
  more_work_items: boolean
  reviews: EvidenceReviewLink[]
  more_reviews: boolean
}

export interface EvidenceCandidatePage {
  items: EvidenceCandidate[]
  next_before: EvidenceCursor | null
}

export interface EvidenceCandidateFilters {
  q?: string
  /** The node that owns this page; remote runners require `channel`. */
  runner?: string
  channel?: string
  sessionId?: string
  reviewId?: string
  workItemId?: string
  limit?: number
  before?: EvidenceCursor
}

/** Read persisted candidate evidence. The node bounds the page and only
 * returns task, session, and review relationships it recorded. */
export function listCandidates(filters: EvidenceCandidateFilters = {}) {
  const query = new URLSearchParams()
  if (filters.q?.trim()) query.set('q', filters.q.trim())
  if (filters.runner) query.set('runner', filters.runner)
  if (filters.channel) query.set('channel', filters.channel)
  if (filters.sessionId) query.set('session_id', filters.sessionId)
  if (filters.reviewId) query.set('review_id', filters.reviewId)
  if (filters.workItemId) query.set('work_item_id', filters.workItemId)
  if (filters.before) {
    query.set('before_ms', String(filters.before.captured_ms))
    query.set('before_id', filters.before.id)
  }
  const suffix = query.size ? `?${query}` : ''
  return call<EvidenceCandidatePage>('GET', `/api/evidence/candidates${suffix}`)
}
