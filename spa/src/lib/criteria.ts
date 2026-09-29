// Reading acceptance criteria in the interface.
//
// The node does the reading: which criteria there are, what points at each
// one, what the checks said, and who has judged it. None of that is redone
// here. What is here is how a state is shown — and the one distinction the
// whole screen exists to keep, said in words rather than in a colour: checks
// passing is not a person agreeing that the criterion was met.

import type { Coverage, Criteria, Criterion, CriterionLink, LinkKind, Standard, Verdict } from './types'

export const LINK_KINDS: LinkKind[] = ['check', 'scenario', 'observation']

export const VERDICTS: Verdict[] = ['met', 'not_met', 'unclear']

/** Whether the standard itself is agreed, or is still somebody's proposal. */
export const STANDARD: Record<Standard, { label: string; title: string }> = {
  agreed: { label: 'agreed', title: 'the customer was observed needing it, or the operator decided it' },
  proposed: { label: 'proposed', title: "an agent's idea of what good means; nobody has agreed to it" },
  unattributed: { label: 'unattributed', title: 'written with no marker, so it says nothing about who is behind it' },
}

/** What a coverage state means, in the operator's terms rather than as a name. */
export const COVERAGE: Record<Coverage, { label: string; title: string }> = {
  judged_met: { label: 'judged met', title: 'a person looked at this candidate and said so' },
  judged_not_met: { label: 'judged not met', title: 'a person looked at this candidate and said so' },
  judged_unclear: { label: 'judged unclear', title: 'a person could not judge the criterion as written' },
  failing: { label: 'failing', title: "an agreed check's latest run for this candidate did not pass" },
  no_result_yet: { label: 'no result yet', title: 'an agreed check has not produced a result for this candidate' },
  checks_pass: { label: 'checks pass · unjudged', title: 'every agreed check passed, and nobody has said it was met' },
  awaits_judgement: {
    label: 'awaits a person',
    title: 'something agreed points at it, but nothing that can produce a result on its own',
  },
  only_proposed: { label: 'only proposed', title: "links exist, and none of them is the operator's" },
  nothing_points_at_it: { label: 'nothing points at it', title: 'no check, scenario or observation is named' },
}

/** Whether a person has spoken. Green is not agreement. */
export function judged(coverage: Coverage): boolean {
  return coverage === 'judged_met' || coverage === 'judged_not_met' || coverage === 'judged_unclear'
}

/** The state to lead with, so a screenful reads at a glance. */
export function attention(coverage: Coverage): 'gap' | 'fail' | 'wait' | 'settled' {
  if (coverage === 'nothing_points_at_it' || coverage === 'only_proposed') return 'gap'
  if (coverage === 'failing' || coverage === 'judged_not_met') return 'fail'
  if (coverage === 'judged_met') return 'settled'
  return 'wait'
}

/** What one link says of itself: its kind, its standing, and its result or why it has none. */
export function linkSays(link: CriterionLink): string {
  const parts: string[] = [link.kind]
  if (link.unresolved) parts.push(link.unresolved)
  else if (link.outcome) parts.push(link.outcome)
  else if (link.kind === 'observation') parts.push('context for a person, not a result')
  else parts.push('no run against this candidate')
  return parts.join(' · ')
}

/** The criteria the caller asked to see, by key. */
export function byKey(view: Criteria, keys: string[]): Criterion[] {
  return keys.map((key) => view.criteria.find((c) => c.key === key)).filter((c): c is Criterion => !!c)
}

/**
 * What the operator still has to do, said as a sentence rather than as a count
 * of rows. Empty when every criterion has been judged for this candidate.
 */
export function whatIsLeft(view: Criteria): string {
  if (view.criteria.length === 0) return view.absent ?? 'no success criteria are stated'
  const uncovered = view.gaps.uncovered.length
  const unjudged = view.gaps.unjudged.length
  if (uncovered === 0 && unjudged === 0) return 'every criterion has been judged for this candidate'
  const parts: string[] = []
  if (uncovered > 0) parts.push(`${uncovered} with nothing agreed pointing at ${uncovered === 1 ? 'it' : 'them'}`)
  if (unjudged > 0) parts.push(`${unjudged} nobody has judged`)
  return parts.join(', ')
}
