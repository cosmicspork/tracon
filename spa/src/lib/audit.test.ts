import { expect, test } from 'bun:test'
import { auditSummary, decidedBy, decisionLabel, outcomeNote } from './audit'
import type { AuditCall } from './types'

const call = (over: Partial<AuditCall>): AuditCall => ({
  at_ms: 1,
  tool: 'doc_write',
  arguments: {},
  decision: 'policy_allowed',
  decided_by: 'policy',
  ...over,
})

test('each decision reads as words, the operator’s edit apart from a plain approval', () => {
  expect(decisionLabel(call({ decision: 'policy_allowed' }))).toEqual(['allowed by policy', 'ok'])
  expect(decisionLabel(call({ decision: 'approved' }))).toEqual(['approved', 'ok'])
  expect(decisionLabel(call({ decision: 'edited' }))).toEqual(['approved with edits', 'wait'])
  expect(decisionLabel(call({ decision: 'denied' }))[1]).toBe('crit')
  expect(decisionLabel(call({ decision: 'pending' }))[0]).toBe('waiting on you')
})

test('who decided names the rule for the policy and the place for the operator', () => {
  expect(decidedBy(call({ decided_by: 'policy', rule: 'docs-read' }))).toBe('policy · docs-read')
  expect(decidedBy(call({ decided_by: 'grant', rule: 'g-1' }))).toBe('grant · g-1')
  expect(decidedBy(call({ decided_by: 'operator (at the node)', rule: 'always-asked' }))).toBe('operator (at the node)')
  expect(decidedBy(call({ decided_by: null }))).toBe('nobody yet')
})

test('the note under a call says why it did not run, or how its run ended', () => {
  expect(outcomeNote(call({ decision: 'denied', reason: 'not that one' }))).toBe('not that one')
  expect(outcomeNote(call({ decision: 'approved', approval_state: 'failed', reason: 'jira said no' }))).toBe('failed · jira said no')
  expect(outcomeNote(call({ decision: 'approved', approval_state: 'succeeded' }))).toBe('')
  expect(outcomeNote(call({ decision: 'changes_requested', note: 'shorter' }))).toBe('shorter')
  expect(outcomeNote(call({ outcome: 'error', error: 'no such doc' }))).toBe('no such doc')
})

test('the summary counts what the operator decided and what was refused', () => {
  expect(auditSummary([])).toBe('no calls to the node yet')
  expect(
    auditSummary([
      call({}),
      call({ decision: 'approved' }),
      call({ decision: 'edited' }),
      call({ decision: 'denied' }),
      call({ decision: 'pending' }),
    ]),
  ).toBe('5 calls · 2 decided by you · 1 refused · 1 waiting')
})
