import type { Blocker, WorkView } from './types'

/** The row's state word, as the ledger shows it. */
export function workState(v: WorkView): 'ready' | 'blocked' | 'insession' | 'closed' {
  if (v.readiness.state === 'closed') return 'closed'
  if (v.readiness.state === 'blocked') return 'blocked'
  return v.session_id ? 'insession' : 'ready'
}

export function workLabel(v: WorkView): string {
  return { ready: 'Ready', blocked: 'Blocked', insession: 'In session', closed: 'Closed' }[workState(v)]
}

/** "waits on b07d13e8 Metrics rollups" — blockers by id and, when known, title. */
export function blockersLine(by: Blocker[], titles: Map<string, string>): string {
  return by
    .map((b) =>
      b.kind === 'cycle'
        ? 'part of a dependency cycle'
        : b.kind === 'unknown'
          ? `waits on ${b.id.slice(0, 8)} (not seen on this node)`
          : `waits on ${b.id.slice(0, 8)}${titles.has(b.id) ? ` ${titles.get(b.id)}` : ''}`,
    )
    .join(' · ')
}

export function short(id: string | null | undefined, n = 8): string {
  return id ? id.slice(0, n) : ''
}
