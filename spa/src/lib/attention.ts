// What "waiting on you" is allowed to mean.
//
// The count used to be the length of everything parked in the operator's bay:
// requests, reviews, promotions, issue drafts. Most of it was not a decision.
// A review the agent is revising, a publication already in flight, a request
// on a node the hub cannot reach — none of those move because a person looks
// at them, and a badge that counts them teaches its reader to ignore it.
//
// So every waiting thread is sorted into one of three lanes and only the first
// is counted. Nothing is dropped: the other two keep their durable threads,
// each carrying the reason it is not yours, so a thread is never lost to make
// a number look better.

import { unreachableReason } from './nodes'
import type {
  MeshState,
  NodeInfo,
  OperatorIssue,
  OperatorQuestion,
  Permission,
  Promotion,
  Review,
} from './types'

export type Lane =
  /** A person can decide this now, and nothing else will. */
  | 'decision'
  /** The agent holds it: revising after changes were asked for, or carrying
   *  on after a request lapsed into its default denial. */
  | 'agent'
  /** Something outside this node holds it: a publication in flight, an
   *  outcome to reconcile, a node that has to come back first. */
  | 'external'

interface Base {
  /** Stable across frames, so a list keyed by it does not re-mount. */
  key: string
  lane: Lane
  /** Why it is not yours to decide. Null on the decision lane. */
  reason: string | null
  created_ms: number
}

export type Thread =
  | (Base & { kind: 'question'; question: OperatorQuestion })
  | (Base & { kind: 'permission'; permission: Permission })
  | (Base & { kind: 'review'; review: Review })
  | (Base & { kind: 'issue'; issue: OperatorIssue })
  | (Base & { kind: 'promotion'; promotion: Promotion })

export interface Attention {
  /** The number the badge and the heading show: actionable human decisions. */
  count: number
  decisions: Thread[]
  agent: Thread[]
  external: Thread[]
}

export interface AttentionInput {
  questions?: OperatorQuestion[]
  permissions?: Permission[]
  reviews?: Review[]
  issues?: OperatorIssue[]
  promotions?: Promotion[]
  nodes?: NodeInfo[]
  mesh?: MeshState | null
  now?: number
}

/** Requests expire, so they are answered before reviews; the node already
 *  orders each list oldest first and this preserves that within a kind. */
const RANK: Record<Thread['kind'], number> = {
  question: 0,
  permission: 1,
  review: 2,
  issue: 3,
  promotion: 4,
}

export interface Verdict {
  lane: Lane
  reason: string | null
}

const YOURS: Verdict = { lane: 'decision', reason: null }

/**
 * A permission request. `held` is the reason its owning node cannot be
 * reached, or null.
 *
 * Expiry is a denial, not a deferral: once a request lapses the node rejects
 * it once on the operator's behalf, so it stops being a decision at the
 * deadline rather than when the sweep gets to it.
 */
export function permissionVerdict(p: Permission, held: string | null, now: number): Verdict {
  if (held !== null) return { lane: 'external', reason: `${held} · cannot be decided until it returns` }
  if (p.expires_ms <= now) {
    return { lane: 'agent', reason: 'expired unanswered · denied by default' }
  }
  const state = p.intent?.session_state
  if (state === 'closed' || state === 'killed_budget' || state === 'failed') {
    return { lane: 'agent', reason: 'session ended · denied by default, nothing left to allow' }
  }
  return YOURS
}

/** A review or narrative report. `held` as above. */
export function reviewVerdict(r: Review, held: string | null): Verdict {
  if (held !== null) return { lane: 'external', reason: `${held} · cannot be decided until it returns` }
  if (r.state === 'revising') return { lane: 'agent', reason: 'changes requested · the agent is revising' }
  if (r.state === 'publishing') {
    return { lane: 'external', reason: 'publication in flight · reconcile the outcome before retrying' }
  }
  // `claimed` is this operator reading it. It is still theirs to decide.
  return YOURS
}

/** An issue draft. Returns null when it is done and belongs in no lane. */
export function issueVerdict(i: OperatorIssue): Verdict | null {
  if (i.state === 'published') return null
  if (i.state === 'publishing') return { lane: 'external', reason: 'publication in flight' }
  if (i.state === 'uncertain') {
    return { lane: 'external', reason: 'publication outcome uncertain · reconcile before retrying' }
  }
  return YOURS
}

/** Every waiting thread, sorted into its lane. */
export function attention(input: AttentionInput): Attention {
  const now = input.now ?? Date.now()
  const nodes = input.nodes ?? []
  const mesh = input.mesh ?? null
  const held = (nodeId: string) => unreachableReason(nodes, mesh, nodeId)
  const threads: Thread[] = []

  for (const question of input.questions ?? []) {
    if (question.state !== 'unanswered') continue
    threads.push({
      key: `question:${question.id}`,
      kind: 'question',
      question,
      created_ms: question.created_ms,
      ...YOURS,
    })
  }
  for (const permission of input.permissions ?? []) {
    threads.push({
      key: `permission:${permission.id}`,
      kind: 'permission',
      permission,
      created_ms: permission.created_ms,
      ...permissionVerdict(permission, held(permission.node_id), now),
    })
  }
  for (const review of input.reviews ?? []) {
    threads.push({
      key: `review:${review.id}`,
      kind: 'review',
      review,
      created_ms: review.created_ms,
      ...reviewVerdict(review, held(review.node_id)),
    })
  }
  for (const issue of input.issues ?? []) {
    const verdict = issueVerdict(issue)
    if (!verdict) continue
    threads.push({
      key: `issue:${issue.id}`,
      kind: 'issue',
      issue,
      created_ms: issue.created_ms,
      ...verdict,
    })
  }
  for (const promotion of input.promotions ?? []) {
    if (promotion.state !== 'open') continue
    threads.push({
      key: `promotion:${promotion.id}`,
      kind: 'promotion',
      promotion,
      created_ms: promotion.created_ms,
      ...YOURS,
    })
  }

  threads.sort((a, b) => RANK[a.kind] - RANK[b.kind] || a.created_ms - b.created_ms)
  const lane = (l: Lane) => threads.filter((t) => t.lane === l)
  const decisions = lane('decision')
  return { count: decisions.length, decisions, agent: lane('agent'), external: lane('external') }
}

/** What a permission card says the session was started to do. */
export function intentLabel(p: Permission): string {
  const intent = p.intent
  if (!intent) return 'intent unavailable'
  const parts = [
    intent.work_item_title ?? 'no work item',
    intent.phase,
    intent.channel,
    intent.branch,
  ].filter((part): part is string => Boolean(part))
  return parts.join(' · ')
}
