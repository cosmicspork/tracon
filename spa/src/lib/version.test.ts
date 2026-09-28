import { expect, test } from 'bun:test'
import { staleInterface } from './version'

test('a page is stale only when both releases are known and differ', () => {
  expect(staleInterface('0.21.0', '0.22.0')).toBe(true)
  expect(staleInterface('0.22.0', '0.22.0')).toBe(false)
  expect(staleInterface('', '0.22.0')).toBe(false)
  expect(staleInterface('0.22.0', undefined)).toBe(false)
  expect(staleInterface('0.22.0', null)).toBe(false)
})
