import { expect, test } from 'bun:test'
import { attemptState, carries, closeOffer, verbs } from './continuation'
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
  expect(verbs({ ...base, actions: { continue_from: null, abandon: false, close: false } })).toEqual({
    continue: false,
    changeApproach: false,
    abandon: false,
  })
})

test('closing the task is offered while it is open, prominently once its request is merged', () => {
  const base = {
    next: { kind: 'merge', text: 'PR #149 is open · merge it', session_id: 's' },
    actions: { continue_from: 's', abandon: true, close: true },
    follow: { url: 'u', label: 'PR #149', state: 'open', failed: [], new_comments: 0, changes_requested: false },
  } as unknown as Continuation
  expect(closeOffer(base)).toEqual({ prominent: false, text: null })
  expect(closeOffer({ ...base, follow: { ...base.follow!, state: 'merged' } })).toEqual({
    prominent: true,
    text: 'PR #149 merged',
  })
  expect(closeOffer({ ...base, follow: null })).toEqual({ prominent: false, text: null })
  expect(closeOffer({ ...base, actions: { ...base.actions, close: false } })).toBe(null)
})
