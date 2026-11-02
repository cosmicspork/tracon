// A session's audit: each call it made to the node's tools, how it was
// decided and by whom. The node decides and records; this only words it.

import type { AuditCall } from './types'

export type AuditTone = 'ok' | 'crit' | 'wait' | 'mark'

/** The decision in a few words, and how loudly to show it. */
export function decisionLabel(call: Pick<AuditCall, 'decision'>): [string, AuditTone] {
  switch (call.decision) {
    case 'policy_allowed':
      return ['allowed by policy', 'ok']
    case 'node_allowed':
      return ['run by the node', 'ok']
    case 'approved':
      return ['approved', 'ok']
    case 'edited':
      return ['approved with edits', 'wait']
    case 'denied':
      return ['denied by policy', 'crit']
    case 'refused':
      return ['refused', 'crit']
    case 'rejected':
      return ['rejected', 'crit']
    case 'changes_requested':
      return ['changes requested', 'wait']
    case 'expired':
      return ['expired unanswered', 'mark']
    case 'pending':
    case 'asked':
      return ['waiting on you', 'wait']
    default:
      return [String(call.decision), 'mark']
  }
}

/** Who decided it, with the rule or grant that did when there is one. */
export function decidedBy(call: Pick<AuditCall, 'decided_by' | 'rule'>): string {
  const who = call.decided_by
  if (!who) return 'nobody yet'
  const rule = call.rule ? ` · ${call.rule}` : ''
  if (who === 'policy' || who === 'grant' || who === 'node') return `${who}${rule}`
  return who
}

/** What became of a call that ran, or could not: the line under it. */
export function outcomeNote(call: AuditCall): string {
  const state = call.approval_state
  if (state === 'failed' || state === 'uncertain') {
    return [state === 'uncertain' ? 'outcome unknown' : 'failed', call.reason ?? ''].filter(Boolean).join(' · ')
  }
  if (state === 'running') return 'running'
  if (call.decision === 'rejected' || call.decision === 'denied' || call.decision === 'refused') {
    return call.reason ?? call.error ?? ''
  }
  if (call.decision === 'changes_requested') return call.note ?? ''
  if (call.outcome === 'error') return call.error ?? 'failed'
  return ''
}

/** Counts by how the calls were decided, for the panel's summary line. */
export function auditSummary(calls: AuditCall[]): string {
  if (!calls.length) return 'no calls to the node yet'
  const byYou = calls.filter((c) => ['approved', 'edited', 'rejected', 'changes_requested'].includes(c.decision)).length
  const refused = calls.filter((c) => c.decision === 'denied' || c.decision === 'refused').length
  const waiting = calls.filter((c) => c.decision === 'pending' || c.decision === 'asked').length
  const parts = [`${calls.length} call${calls.length === 1 ? '' : 's'}`]
  if (byYou) parts.push(`${byYou} decided by you`)
  if (refused) parts.push(`${refused} refused`)
  if (waiting) parts.push(`${waiting} waiting`)
  return parts.join(' · ')
}
