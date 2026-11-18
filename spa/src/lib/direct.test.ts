import { expect, test } from 'bun:test'
import { directMarker, directWarning, parseTarget, protectionLine } from './direct'

test('a direct push is warned of only while warn_direct is on', () => {
  const on = parseTarget('{"base":"main","branch":"main","direct":true,"warn_direct":true}')
  expect(directWarning(on, 'github')).toBe('Pushes straight to main, no pull request; CI runs after the push')
  expect(directWarning(on, 'gitlab')).toBe('Pushes straight to main, no merge request; CI runs after the push')
  expect(directMarker(on)).toBe('pushes straight to main')
  const off = parseTarget('{"base":"main","branch":"main","direct":true}')
  expect(directWarning(off, 'github')).toBeNull()
  expect(directMarker(off)).toBeNull()
  expect(directWarning(parseTarget('{"base":"main","branch":"feat/x"}'), 'github')).toBeNull()
  expect(parseTarget('not json')).toBeNull()
})

test('protection says what it found, a coming rejection, or that it is unknown', () => {
  expect(protectionLine(null)).toBeNull()
  expect(protectionLine({})).toEqual({ tone: 'dim', text: 'not protected' })
  expect(protectionLine({ found: ['required checks spa, title'] })).toEqual({
    tone: 'dim',
    text: 'protected: required checks spa, title',
  })
  const rejects = protectionLine({
    found: ['requires a pull request', 'push restricted'],
    rejects: 'main requires a pull request; this push will be rejected. Resubmit on a branch to open a change.',
  })
  expect(rejects?.tone).toBe('crit')
  expect(rejects?.text).toContain('this push will be rejected')
  expect(rejects?.text).toContain('requires a pull request; push restricted')
  expect(protectionLine({ unknown: 'gh: HTTP 403' })).toEqual({ tone: 'wait', text: 'protection unknown · gh: HTTP 403' })
})
