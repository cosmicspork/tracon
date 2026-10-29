import { expect, test } from 'bun:test'
import { checkLabel, claimStanding, headline, noChecks } from './outcome'
import type { OutcomeCheck, SessionOutcome } from './types'

const check = (over: Partial<OutcomeCheck>): OutcomeCheck => ({
  check_id: 'c',
  command: 'just check',
  outcome: 'passed',
  source_outcome: null,
  head_sha: 'a'.repeat(40),
  passed: true,
  failed: false,
  current: true,
  finished_ms: 1,
  ...over,
})

const outcome = (over: Partial<SessionOutcome>): SessionOutcome => ({
  session_id: 's1',
  channel: 'personal',
  state: 'closed',
  end_reason: null,
  phase: 'execute',
  plan: null,
  verdict: null,
  head_sha: 'a'.repeat(40),
  changed: { reviews: [], files: [], added: 0, removed: 0, workspace_changes: 0 },
  verified: [],
  claims: [],
  needs_decision: [],
  uncertain: [],
  cost: {
    tokens_used: 0,
    budget_tokens: 0,
    cost_usd: null,
    gateway_tokens: 0,
    charged_tokens: 0,
    unmetered_turns: 0,
    mismatched_turns: 0,
  },
  ...over,
})

test('the headline leads with what the node verified, failures first', () => {
  expect(headline(outcome({}))).toBe('the latest commit is unverified')
  expect(headline(outcome({ verified: [check({})] }))).toBe('checks pass on the latest commit')
  expect(
    headline(
      outcome({
        verified: [check({}), check({ outcome: 'failed', passed: false, failed: true })],
        uncertain: ['x'],
        needs_decision: [{ kind: 'review', id: 'r', title: 't', since_ms: 0 }],
      }),
    ),
  ).toBe('1 check failing on the latest commit · 1 waiting on you · 1 uncertain')
  expect(headline(outcome({ head_sha: null }))).toBe('nothing submitted for review')
})

test('a plan session is headed by its plan, not by a review it was never asked for', () => {
  const plan = { phase: 'plan' as const, head_sha: null }
  expect(headline(outcome({ ...plan, plan: { slug: 'plan-w1', hash: 'h' } }))).toBe('plan written')
  expect(headline(outcome(plan))).toBe('no plan written')
  expect(
    headline(outcome({ ...plan, needs_decision: [{ kind: 'question', id: 'q', title: 't', since_ms: 0 }] })),
  ).toBe('no plan written · 1 waiting on you')
})

test('a review session is headed by its verdict', () => {
  const verdict = { review_id: 'rv1', summary: null, findings: 0, superseded: false }
  const review = { phase: 'review' as const, head_sha: null }
  expect(headline(outcome(review))).toBe('no verdict given')
  expect(headline(outcome({ ...review, verdict: { ...verdict, verdict: 'approve' } }))).toBe('approves')
  expect(
    headline(outcome({ ...review, verdict: { ...verdict, verdict: 'request_changes', findings: 2 } })),
  ).toBe('requests changes, 2 findings')
  expect(
    headline(outcome({ ...review, verdict: { ...verdict, verdict: 'approve', findings: 1, superseded: true } })),
  ).toBe('approves, 1 finding (superseded)')
})

test('a pass on an earlier commit does not count for the latest', () => {
  expect(headline(outcome({ verified: [check({ current: false })] }))).toBe('the latest commit is unverified')
  expect(checkLabel(check({ current: false }))).toContain('(earlier commit)')
  expect(checkLabel(check({ outcome: 'reused', source_outcome: 'passed' }))).toBe(
    'just check: reused, passed on aaaaaaaaaaaa',
  )
})

test('a claim says what stands behind it', () => {
  const base = { id: 'r', title: 't', text: 'all green', head_sha: 'a' }
  expect(claimStanding({ ...base, source: 'review', backed: false, backed_by: [] })).toContain('word only')
  expect(claimStanding({ ...base, source: 'report', head_sha: null, backed: false, backed_by: [] })).toContain(
    'narrative only',
  )
  expect(claimStanding({ ...base, source: 'review', backed: true, backed_by: ['c1'] })).toBe(
    'backed by 1 passing check on its commit',
  )
})

test('a peer session whose node could not be read is not said to have no checks', () => {
  const unread = outcome({ owner_checks: { node_id: 'n2', read: false, reason: 'the node that holds it is unreachable' } })
  expect(headline(unread)).toBe('its checks could not be read from the node that runs it')
  expect(noChecks(unread)).toContain('could not be read from it: the node that holds it is unreachable')
  expect(noChecks(outcome({ owner_checks: { node_id: 'n2', read: true, reason: null } }))).toBe('No check has run.')
  expect(noChecks(outcome({}))).toBe('No check has run.')
})

test('a published session is headed by its request, as the node last saw it', () => {
  type Request = SessionOutcome['changed']['reviews'][number]['request']
  const change = (id: string, request: Request) => ({
    id,
    kind: 'pr',
    title: id,
    state: 'approved',
    head_sha: 'a'.repeat(40),
    added: 1,
    removed: 0,
    files: 1,
    request,
  })
  const changed = (...reviews: ReturnType<typeof change>[]) => ({
    changed: { reviews, files: [], added: 0, removed: 0, workspace_changes: 0 },
  })
  const url = 'https://github.com/o/r/pull/150'
  const mr = 'https://gitlab.com/o/r/-/merge_requests/9'
  expect(headline(outcome(changed(change('rv1', null))))).toBe('the latest commit is unverified')
  expect(headline(outcome(changed(change('rv1', { url, state: null }))))).toBe(
    'PR #150 opened · the latest commit is unverified',
  )
  expect(
    headline(
      outcome({
        ...changed(change('rv1', { url: mr, state: 'closed' }), change('rv2', { url, state: 'merged' })),
        verified: [check({})],
      }),
    ),
  ).toBe('PR #150 merged · checks pass on the latest commit')
})
