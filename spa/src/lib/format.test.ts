import { expect, test } from 'bun:test'
import { formatAge, formatBudget, formatDuration, formatExpiry, formatTokens } from './format'

test('tokens are shown with a k or M suffix once they earn one', () => {
  expect(formatTokens(512)).toBe('512')
  expect(formatTokens(15024)).toBe('15k')
  expect(formatTokens(1_240_000)).toBe('1.24M')
  expect(formatTokens(2_000_000)).toBe('2M')
})

test('a budget reads as used over total', () => {
  expect(formatBudget(412_000, 2_000_000)).toBe('412k/2M')
})

test('a duration is the largest unit that fits, and only that unit', () => {
  const s = 1000
  const m = 60 * s
  const h = 60 * m
  const d = 24 * h
  expect(formatDuration(48 * s)).toBe('48s')
  expect(formatDuration(12 * m)).toBe('12m')
  // The minutes remainder is deliberately gone: 2h10 asked to be read twice.
  expect(formatDuration(130 * m)).toBe('2h')
  expect(formatDuration(2 * h)).toBe('2h')
  expect(formatDuration(3 * d)).toBe('3d')
  expect(formatDuration(2 * 7 * d)).toBe('2w')
  expect(formatDuration(90 * d)).toBe('3mo')
  expect(formatDuration(400 * d)).toBe('1y')
})

test('each unit hands over at its own boundary', () => {
  const s = 1000
  const m = 60 * s
  const h = 60 * m
  const d = 24 * h
  expect(formatDuration(59 * s)).toBe('59s')
  expect(formatDuration(60 * s)).toBe('1m')
  expect(formatDuration(59 * m)).toBe('59m')
  expect(formatDuration(60 * m)).toBe('1h')
  expect(formatDuration(23 * h)).toBe('23h')
  expect(formatDuration(24 * h)).toBe('1d')
  expect(formatDuration(6 * d)).toBe('6d')
  expect(formatDuration(7 * d)).toBe('1w')
  expect(formatDuration(34 * d)).toBe('4w')
  expect(formatDuration(35 * d)).toBe('1mo')
  expect(formatDuration(364 * d)).toBe('12mo')
  expect(formatDuration(365 * d)).toBe('1y')
})

test('an age is a duration measured back from now, and never negative', () => {
  const now = 1_000_000_000
  expect(formatAge(now - 48_000, now)).toBe('48s')
  expect(formatAge(now - 130 * 60_000, now)).toBe('2h')
  // 139 hours: the shape that read as a session running for six days.
  expect(formatAge(now - 139 * 3_600_000, now)).toBe('5d')
  expect(formatAge(now + 5_000, now)).toBe('0s')
})

test('an expiry counts down and then says so', () => {
  const now = 1_000_000_000
  expect(formatExpiry(now + 240_000, now)).toBe('expires 4m')
  expect(formatExpiry(now + 30_000, now)).toBe('expires 30s')
  expect(formatExpiry(now - 1, now)).toBe('expired')
})
