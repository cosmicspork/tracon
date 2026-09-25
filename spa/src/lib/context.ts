// Reading and editing a work item's selected context in the interface.
//
// The node resolves the selection and records what every attempt received;
// none of that is redone here. What is here is how it is said: the role a
// document plays, why a document will not reach the next attempt, and one
// line per attempt that says whether its context moved.

import type { ContextPick, ContextReason, ContextReceipt, ContextReceived, ContextRole, ContextSelection } from './types'

export const ROLES: ContextRole[] = ['brief', 'research', 'decisions', 'constraints', 'documents']

const HEADINGS: Record<ContextRole, string> = {
  brief: 'Brief',
  research: 'Research',
  decisions: 'Decisions',
  constraints: 'Constraints',
  documents: 'Documents',
}

export function roleHeading(role: ContextRole): string {
  return HEADINGS[role]
}

const REASONS: Record<ContextReason, string> = {
  absent: 'not on this node',
  archived: 'archived',
  html: 'an HTML document, not carried in orientation',
  cap: 'past the selected context’s allowance',
}

export function reasonLine(reason: ContextReason): string {
  return REASONS[reason]
}

/** "delivered in full", "cut short · past the allowance", "left out · not on this node". */
export function deliveryLine(r: ContextReceived): string {
  if (r.delivery === 'full') return 'delivered in full'
  const why = r.reason ? ` · ${reasonLine(r.reason)}` : ''
  return r.delivery === 'partial' ? `cut short${why}` : `left out${why}`
}

/**
 * "revision 2 · 3 changes since revision 1 · 1 left out" — enough to see from
 * the list whether an attempt started from something different, without
 * opening it.
 */
export function attemptLine(a: ContextReceipt): string {
  const parts = [`revision ${a.revision}`]
  if (a.previous_revision === undefined || a.previous_revision === null) {
    parts.push('first attempt')
  } else if (a.changes.length === 0) {
    parts.push('unchanged')
  } else {
    const n = a.changes.length
    parts.push(`${n} ${n === 1 ? 'change' : 'changes'} since revision ${a.previous_revision}`)
  }
  const omitted = a.received.filter((r) => r.delivery !== 'full').length
  if (omitted > 0) parts.push(`${omitted} not delivered whole`)
  if (a.received.length === 0) parts.push('nothing selected')
  return parts.join(' · ')
}

/** The selection's picks, in the order a session reads them. */
export function picksInOrder(selection: ContextSelection | null): ContextPick[] {
  if (!selection) return []
  return ROLES.flatMap((role) => selection.picks.filter((p) => p.role === role))
}

/** Picks as the API takes them, with one removed. */
export function without(picks: ContextPick[], slug: string): ContextPick[] {
  return picks.filter((p) => p.slug !== slug)
}

/**
 * Picks with one added. A slug already selected moves to the new role rather
 * than being selected twice — the node refuses a duplicate, and the operator
 * who picked it again meant the new role.
 */
export function withPick(picks: ContextPick[], pick: ContextPick): ContextPick[] {
  const slug = pick.slug.trim()
  if (!slug) return picks
  const kept = picks.filter((p) => p.slug !== slug)
  return [...kept, { role: pick.role, slug, note: pick.note.trim() }]
}
