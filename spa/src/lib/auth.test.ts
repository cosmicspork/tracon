import { expect, test } from 'bun:test'

import { insecureContext, stashToken, takeToken, tokenFromHash } from './auth'

test('a login fragment yields its token', () => {
  expect(tokenFromHash('#token=trc1.abc-_123')).toBe('trc1.abc-_123')
})

test('anything that is not a lone tracon token is refused', () => {
  expect(tokenFromHash('')).toBeNull()
  expect(tokenFromHash('#enroll=7KQ4M2XA')).toBeNull()
  expect(tokenFromHash('#token=nonsense')).toBeNull()
  expect(tokenFromHash('#token=trc1.x&other=1')).toBeNull()
})

test('the stash is spent once', () => {
  stashToken('trc1.once')
  expect(takeToken()).toBe('trc1.once')
  expect(takeToken()).toBeNull()
})

test('plain http is secure only on the machine itself', () => {
  expect(insecureContext('https:', 'node.example.ts.net')).toBe(false)
  expect(insecureContext('http:', 'localhost')).toBe(false)
  expect(insecureContext('http:', '127.0.0.1')).toBe(false)
  expect(insecureContext('http:', '192.168.1.20')).toBe(true)
})
