import { expect, test } from 'bun:test'
import { exhaustionNote } from './exhaustion'
import type { SessionExhaustion } from './types'

const base: SessionExhaustion = {
  policy: 'pause',
  fallback: null,
  provider: 'anthropic',
  model: 'anthropic/claude-x',
  reason: 'usage limit reached',
  reset_ms: null,
  next_wake_ms: null,
  boundary_seq: 4,
  outcome: null,
  note: null,
  continued_by: null,
}

test('nothing is said before an exhaustion or after it settles', () => {
  expect(exhaustionNote(null)).toBe(null)
  expect(exhaustionNote(base)).toBe(null)
  expect(exhaustionNote({ ...base, outcome: 'resumed' })).toBe(null)
})

test('a waiting session says when it resumes, only if the provider said', () => {
  const note = exhaustionNote({ ...base, outcome: 'waiting', next_wake_ms: 1_791_374_400_000 })
  expect(note?.title).toBe('anthropic is exhausted')
  expect(note?.detail.startsWith('resumes when its limit resets, ')).toBe(true)
})

test('a held session says why it waits for the operator', () => {
  const note = exhaustionNote({ ...base, outcome: 'held', note: 'The provider gave no reset time.' })
  expect(note?.detail).toBe('waiting for you · The provider gave no reset time.')
})

test('a fallback names the model it carries on with', () => {
  const note = exhaustionNote({ ...base, outcome: 'falling_back', policy: 'fallback', fallback: 'openai/gpt-x' })
  expect(note?.detail).toBe('carrying on with openai/gpt-x from where it stopped')
})
