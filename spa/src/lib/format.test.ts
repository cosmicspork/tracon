import { expect, test } from 'bun:test'
import { formatTokens } from './format'

test('formatTokens', () => {
  expect(formatTokens(512)).toBe('512')
  expect(formatTokens(15024)).toBe('15k')
  expect(formatTokens(1_240_000)).toBe('1.24M')
  expect(formatTokens(2_000_000)).toBe('2M')
})
