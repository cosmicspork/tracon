import { expect, test } from 'bun:test'
import { checkLabel, claimStanding, headline } from './outcome'
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
