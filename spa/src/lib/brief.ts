// Reading a brief in the interface.
//
// The node parses the document and marks every line; none of that is redone
// here. What is here is how a marked line is shown, and one piece of parsing
// the node cannot do for the operator: turning what someone typed into the
// references box into references, so that adding a line and citing what it
// rests on is one action rather than two.

import type { Brief, BriefEntry, BriefField, BriefRef, Provenance, RefKind } from './types'

export const FIELDS: BriefField[] = [
  'intended_user',
  'problem',
  'source_references',
  'constraints',
  'success_criteria',
  'unresolved_questions',
]

export const REF_KINDS: RefKind[] = ['doc', 'session', 'evidence', 'work', 'url', 'file']

const HEADINGS: Record<BriefField, string> = {
  intended_user: 'Intended user',
  problem: 'Problem',
  source_references: 'Source references',
  constraints: 'Constraints',
  success_criteria: 'Success criteria',
  unresolved_questions: 'Unresolved questions',
}

export function fieldHeading(field: BriefField): string {
  return HEADINGS[field]
}

/** The word shown against a line, and what it claims. */
export const PROVENANCE: Record<Provenance, { label: string; title: string }> = {
  observed: { label: 'observed', title: 'the customer said or did this' },
  inferred: { label: 'inferred', title: 'someone reasoned to it from something else' },
  decided: { label: 'decided', title: 'the operator chose it' },
  unattributed: { label: 'unattributed', title: 'written with no marker: not evidence, and not a decision' },
}

/** Where a reference leads in the interface, or null where it leads nowhere. */
export function refHref(channel: string, ref: BriefRef): string | null {
  switch (ref.kind) {
    case 'doc':
      return `/docs/${channel}/${ref.value}`
    case 'work':
      return `/work/${ref.value}`
    case 'session':
      return `/sessions/${ref.value}`
    case 'url':
      return ref.value
    default:
      return null
  }
}

/** "meeting-ops · not on this node" — the label, and the gap where there is one. */
export function refLabel(ref: BriefRef): string {
  const name = ref.label ?? ref.value
  return ref.known === false ? `${name} · not on this node` : name
}

/**
 * What someone typed into the references box. `doc:meeting-ops` and a bare
 * URL are both references; anything else is returned as a complaint rather
 * than dropped, so a mistyped citation is seen before the line is written.
 */
export function parseRefs(text: string): { refs: { kind: RefKind; value: string }[]; rejected: string[] } {
  const refs: { kind: RefKind; value: string }[] = []
  const rejected: string[] = []
  for (const token of text.split(/[\s,]+/).filter(Boolean)) {
    const bare = token.replace(/^\[/, '').replace(/\]$/, '')
    if (/^https?:\/\//.test(bare)) {
      refs.push({ kind: 'url', value: bare })
      continue
    }
    const at = bare.indexOf(':')
    const kind = at > 0 ? bare.slice(0, at).toLowerCase() : ''
    const value = at > 0 ? bare.slice(at + 1) : ''
    if (!REF_KINDS.includes(kind as RefKind) || !value) {
      rejected.push(token)
      continue
    }
    if (kind === 'url') {
      rejected.push(token)
      continue
    }
    refs.push({ kind: kind as RefKind, value })
  }
  return { refs, rejected }
}

/** Lines in a section, or an empty list — the caller says what absence means. */
export function entriesOf(brief: Brief, field: BriefField): BriefEntry[] {
  return brief.sections.find((s) => s.field === field)?.entries ?? []
}

/**
 * What the brief rests on, in one line: the counts, and the sections that say
 * nothing yet. A brief of nothing but inferences reads as one.
 */
export function restsOn(brief: Brief): string {
  const c = brief.counts
  const parts = [`${c.observed} observed`, `${c.inferred} inferred`, `${c.decided} decided`]
  if (c.unattributed > 0) parts.push(`${c.unattributed} unattributed`)
  if (brief.absent.length > 0) parts.push(brief.absent.map((a) => a.says).join(', '))
  return parts.join(' · ')
}

/** True when nothing in the brief was observed: it is all reasoning so far. */
export function nothingObserved(brief: Brief): boolean {
  const c = brief.counts
  return c.observed === 0 && c.inferred + c.decided + c.unattributed > 0
}
