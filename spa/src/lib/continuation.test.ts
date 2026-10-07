import { expect, test } from 'bun:test'
import { attemptState, carries, verbs } from './continuation'
import type { Continuation, ContinuationAttempt } from './types'

const attempt = (over: Partial<ContinuationAttempt>): ContinuationAttempt => ({
  id: 'aaaaaaaa-1',
  phase: 'execute',
  model: 'm/a',
  harness: 'claude',
  state: 'closed',
  end_reason: null,
  last_error: null,
  tokens_used: 0,
  created_ms: 1,
  continued_from: null,
  parent_session: null,
  archived: false,
  ...over,
})

test('an attempt says how it ended, and why when it failed', () => {
  expect(attemptState(attempt({ end_reason: 'node_restart' }))).toBe('node restarted')
  expect(attemptState(attempt({ end_reason: 'error', last_error: 'tests red' }))).toBe('failed: tests red')
  expect(attemptState(attempt({ end_reason: 'something_new' }))).toBe('something new')
  expect(attemptState(attempt({ state: 'waiting_on_you' }))).toBe('waiting on you')
})

test('lineage points only at attempts in the same work', () => {
  const a = attempt({ id: 'aaaaaaaa-1' })
  const b = attempt({ id: 'bbbbbbbb-2', continued_from: 'aaaaaaaa-1' })
  const stray = attempt({ id: 'cccccccc-3', parent_session: 'elsewhere' })
  expect(carries(a, [a, b])).toBe(null)
  expect(carries(b, [a, b])).toBe('aaaaaaaa')
  expect(carries(stray, [a, stray])).toBe(null)
})

test('the verbs follow what the node allows', () => {
  const base = {
    next: { kind: 'continue', text: '', session_id: 'b' },
    actions: { continue_from: 'b', abandon: true },
  } as unknown as Continuation
  expect(verbs(base)).toEqual({ continue: true, changeApproach: true, abandon: true })
  expect(verbs({ ...base, next: { ...base.next, kind: 'change_approach' } })).toEqual({
    continue: false,
    changeApproach: true,
    abandon: true,
  })
  expect(verbs({ ...base, actions: { continue_from: null, abandon: false } })).toEqual({
    continue: false,
    changeApproach: false,
    abandon: false,
  })
})
