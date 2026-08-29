import { expect, test } from 'bun:test'
import { formatAge, formatBudget, formatExpiry, formatTokens } from './format'

test('tokens are shown with a k or M suffix once they earn one', () => {
  expect(formatTokens(512)).toBe('512')
  expect(formatTokens(15024)).toBe('15k')
  expect(formatTokens(1_240_000)).toBe('1.24M')
  expect(formatTokens(2_000_000)).toBe('2M')
})

test('a budget reads as used over total', () => {
  expect(formatBudget(412_000, 2_000_000)).toBe('412k/2M')
})

test('an age is the largest unit that fits, with minutes only under a day', () => {
  const now = 1_000_000_000
  expect(formatAge(now - 48_000, now)).toBe('48s')
  expect(formatAge(now - 12 * 60_000, now)).toBe('12m')
  expect(formatAge(now - 130 * 60_000, now)).toBe('2h10')
  expect(formatAge(now - 120 * 60_000, now)).toBe('2h')
})

test('an expiry counts down and then says so', () => {
  const now = 1_000_000_000
  expect(formatExpiry(now + 240_000, now)).toBe('expires 4m')
  expect(formatExpiry(now + 30_000, now)).toBe('expires 30s')
  expect(formatExpiry(now - 1, now)).toBe('expired')
})
