// Which verdicts a review screen can send right now, and in words why not.
// A disabled button with no reason is a puzzle; every action that cannot be
// taken says so beside it instead.

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
