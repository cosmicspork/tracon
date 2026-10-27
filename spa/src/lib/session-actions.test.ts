import { expect, test } from 'bun:test'
import { askMode, compactOffered, composerState, sessionControls } from './session-actions'
import type { Session } from './types'

test('a suspended or ended session offers no Pause or Stop', () => {
  expect(sessionControls({ state: 'suspended' })).toEqual({ pause: false, resume: false, stop: false })
  expect(sessionControls({ state: 'closed' })).toEqual({ pause: false, resume: false, stop: false })
  expect(sessionControls({ state: 'failed' })).toEqual({ pause: false, resume: false, stop: false })
})

test('a live session offers what its state takes', () => {
  expect(sessionControls({ state: 'running' })).toEqual({ pause: true, resume: false, stop: true })
  expect(sessionControls({ state: 'starting' })).toEqual({ pause: false, resume: false, stop: true })
  expect(sessionControls({ state: 'paused' })).toEqual({ pause: false, resume: true, stop: true })
  // Held by an exhausted provider: Resume is in the banner, not twice.
  expect(sessionControls({ state: 'paused' }, true)).toEqual({ pause: false, resume: false, stop: true })
})

test('the prompt box says it is disabled only when it is', () => {
  expect(composerState(null, false)).toEqual({
    disabled: false,
    placeholder: 'Send a prompt. Drafts are held on the node.',
    queues: false,
  })
  expect(composerState('paused', false)).toEqual({ disabled: true, placeholder: 'Input disabled: paused', queues: false })
})

test('a running turn takes a message to queue rather than refusing it', () => {
  const typing = composerState('a turn is running', true)
  expect(typing.disabled).toBe(false)
  expect(typing.queues).toBe(true)
  expect(typing.placeholder).not.toContain('disabled')
  expect(typing.placeholder).toBe(
    'A turn is running. Queue a message: the agent gets it in its next tracon tool result, or when the turn ends.',
  )
})

const can = { restorable: true, reason: null, compacts: true }
const cannot = { restorable: false, reason: 'its workspace was removed', compacts: true }

test('an ended or suspended session is asked again where its conversation can be restored', () => {
  const ended: Pick<Session, 'state' | 'harness_id' | 'legacy_ms'> = { state: 'closed', harness_id: 'claude', legacy_ms: null }
  expect(askMode(ended, can)).toEqual({ kind: 'ask' })
  expect(askMode({ ...ended, state: 'suspended' }, can)).toEqual({ kind: 'ask' })
  expect(askMode(ended, cannot)).toEqual({ kind: 'unrestorable', reason: 'its workspace was removed' })
  // A live session has its own box, and nothing is said before the node answers.
  expect(askMode({ ...ended, state: 'running' }, can)).toEqual({ kind: 'live' })
  expect(askMode(ended, null)).toEqual({ kind: 'none' })
  expect(askMode({ ...ended, harness_id: 'external' }, can)).toEqual({ kind: 'none' })
  expect(askMode({ ...ended, legacy_ms: 1 }, can)).toEqual({ kind: 'none' })
})

test('Compact is offered on an idle live session or one that can be asked again', () => {
  expect(compactOffered({ state: 'running', turn_active: 0 }, can)).toBe(true)
  expect(compactOffered({ state: 'running', turn_active: 1 }, can)).toBe(false)
  expect(compactOffered({ state: 'closed', turn_active: 0 }, can)).toBe(true)
  expect(compactOffered({ state: 'suspended', turn_active: 0 }, can)).toBe(true)
  expect(compactOffered({ state: 'closed', turn_active: 0 }, cannot)).toBe(false)
  expect(compactOffered({ state: 'paused', turn_active: 0 }, can)).toBe(false)
  expect(compactOffered({ state: 'running', turn_active: 0 }, { ...can, compacts: false })).toBe(false)
  expect(compactOffered({ state: 'running', turn_active: 0 }, null)).toBe(false)
})
