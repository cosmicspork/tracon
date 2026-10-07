import { expect, test } from 'bun:test'
import { countLabel, formatBytes } from './data'

test('a size reads in the unit a person expects', () => {
  expect(formatBytes(0)).toBe('0 B')
  expect(formatBytes(-4)).toBe('0 B')
  expect(formatBytes(812)).toBe('812 B')
  expect(formatBytes(4_200)).toBe('4.2 KB')
  expect(formatBytes(56_000_000)).toBe('56 MB')
  expect(formatBytes(1_300_000_000)).toBe('1.3 GB')
})

test('a count names its unit', () => {
  expect(countLabel(1, 'session')).toBe('1 session')
  expect(countLabel(12, 'work item')).toBe('12 work items')
  expect(countLabel(0, 'memory')).toBe('no memories')
  expect(countLabel(2_500, 'event')).toBe('2,500 events')
})
