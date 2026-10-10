import { expect, test } from 'bun:test'
import { sessionTone } from './tone'

test('an ended session is green only when its work landed', () => {
  expect(sessionTone({ state: 'closed', end_reason: 'phase_done' })).toBe('ok')
  expect(sessionTone({ state: 'closed', end_reason: 'item_close' })).toBe('ok')
  expect(sessionTone({ state: 'closed', end_reason: null })).toBe('ok')
})

test('an ended session that neither failed nor finished is quiet', () => {
  for (const end_reason of ['killed_user', 'detached', 'continued', 'node_restart', 'provider_exhausted']) {
    expect(sessionTone({ state: 'closed', end_reason })).toBe('dim')
  }
})

test('failure is red whether the state or the end reason says so', () => {
  expect(sessionTone({ state: 'failed', end_reason: null })).toBe('crit')
  expect(sessionTone({ state: 'killed_budget', end_reason: 'budget' })).toBe('crit')
  expect(sessionTone({ state: 'closed', end_reason: 'error' })).toBe('crit')
  expect(sessionTone({ state: 'closed', end_reason: 'incompatible' })).toBe('crit')
})

test('a live session is waiting on you or running', () => {
  expect(sessionTone({ state: 'waiting_on_you', end_reason: null })).toBe('wait')
  expect(sessionTone({ state: 'paused', end_reason: null })).toBe('wait')
  expect(sessionTone({ state: 'running', end_reason: null })).toBe('run')
  expect(sessionTone({ state: 'suspended', end_reason: null })).toBe('run')
})
