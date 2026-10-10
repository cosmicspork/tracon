import { expect, test } from 'bun:test'
import { composerState, sessionControls } from './session-actions'

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
  expect(composerState(null, false)).toEqual({ disabled: false, placeholder: 'Send a prompt. Drafts are held on the node.' })
  expect(composerState('paused', false)).toEqual({ disabled: true, placeholder: 'Input disabled: paused' })
  const typing = composerState('a turn is running', true)
  expect(typing.disabled).toBe(false)
  expect(typing.placeholder).not.toContain('disabled')
  expect(typing.placeholder).toBe('A turn is running. Type the next prompt; Send opens when that clears.')
})
