import { expect, test } from 'bun:test'
import { COVERAGE, STANDARD, attention, byKey, judged, linkSays, whatIsLeft } from './criteria'
import type { Coverage, Criteria, Criterion, CriterionLink } from './types'

function criterion(partial: Partial<Criterion> = {}): Criterion {
  return {
    key: 'sc-000000000000',
    text: 'Triage finishes in under two minutes',
    provenance: 'decided',
    standard: 'agreed',
    refs: [],
    links: [],
    coverage: 'nothing_points_at_it',
    ...partial,
  }
}

function view(partial: Partial<Criteria> = {}): Criteria {
  return {
    work_item_id: 'item-1',
    channel: 'personal',
    brief_slug: 'brief-item-1',
    hash: 'h',
    criteria: [],
    gaps: { uncovered: [], unjudged: [], assumptions: [], questions: [], orphaned_judgements: [] },
    summary: '',
    ...partial,
  }
}

function link(partial: Partial<CriterionLink> = {}): CriterionLink {
  return { index: 0, provenance: 'decided', standard: 'agreed', kind: 'check', value: 'just check', refs: [], ...partial }
}

test('passing checks is never shown as a person agreeing', () => {
  expect(COVERAGE.checks_pass.label).toBe('checks pass · unjudged')
  expect(judged('checks_pass')).toBe(false)
  expect(judged('judged_met')).toBe(true)
  // Every state the node can produce says something; a missing label would
  // show a criterion as blank, which reads as covered.
  const states: Coverage[] = [
    'judged_met',
    'judged_not_met',
    'judged_unclear',
    'failing',
    'no_result_yet',
    'checks_pass',
    'awaits_judgement',
    'only_proposed',
    'nothing_points_at_it',
  ]
  for (const state of states) {
    expect(COVERAGE[state].label.length).toBeGreaterThan(0)
    expect(COVERAGE[state].title.length).toBeGreaterThan(0)
  }
})

test("an agent's standard is shown as a proposal, not as the standard", () => {
  expect(STANDARD.proposed.label).toBe('proposed')
  expect(STANDARD.proposed.title).toContain('nobody has agreed')
  expect(STANDARD.agreed.title).toContain('operator decided it')
})

test('what needs attention is a gap or a failure, never a green', () => {
  expect(attention('nothing_points_at_it')).toBe('gap')
  expect(attention('only_proposed')).toBe('gap')
  expect(attention('failing')).toBe('fail')
  expect(attention('judged_not_met')).toBe('fail')
  expect(attention('judged_met')).toBe('settled')
  // Checks passing is waiting on a person, not done.
  expect(attention('checks_pass')).toBe('wait')
  expect(attention('awaits_judgement')).toBe('wait')
})

test('a link says what it is and why it has no result', () => {
  expect(linkSays(link({ outcome: 'passed' }))).toBe('check · passed')
  expect(linkSays(link())).toBe('check · no run against this candidate')
  expect(linkSays(link({ kind: 'scenario', unresolved: 'this node holds no scenario records yet' }))).toBe(
    'scenario · this node holds no scenario records yet',
  )
  expect(linkSays(link({ kind: 'observation', provenance: 'observed' }))).toBe(
    'observation · context for a person, not a result',
  )
})

test('what is left says the gaps, and says when there are none to say', () => {
  expect(whatIsLeft(view({ absent: 'nothing is said about what would make this good' }))).toBe(
    'nothing is said about what would make this good',
  )
  const one = criterion({ coverage: 'judged_met' })
  expect(whatIsLeft(view({ criteria: [one] }))).toBe('every criterion has been judged for this candidate')
  expect(
    whatIsLeft(
      view({
        criteria: [one, criterion({ key: 'sc-a', coverage: 'nothing_points_at_it' })],
        gaps: { uncovered: ['sc-a'], unjudged: ['sc-a'], assumptions: [], questions: [], orphaned_judgements: [] },
      }),
    ),
  ).toBe('1 with nothing agreed pointing at it, 1 nobody has judged')
})

test('the gap lists name criteria the view actually holds', () => {
  const held = criterion({ key: 'sc-a' })
  const v = view({ criteria: [held] })
  expect(byKey(v, ['sc-a']).map((c) => c.key)).toEqual(['sc-a'])
  // A key the view does not hold is dropped rather than rendered as a blank row.
  expect(byKey(v, ['sc-a', 'sc-gone'])).toHaveLength(1)
})
