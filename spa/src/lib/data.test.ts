import { expect, test } from 'bun:test'
import { countLabel, formatBytes, resetRefusal, serviceVolumeLine } from './data'

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

test('a service volume says what it holds and how it stands', () => {
  const now = 10 * 3_600_000
  const volume = { service: 'postgres', bytes: 41_000_000, used_ms: now - 3 * 3_600_000, live: false, mounted: true }
  expect(serviceVolumeLine(volume, now)).toBe('postgres · 41 MB · last used 3 h ago')
  expect(serviceVolumeLine({ ...volume, live: true }, now)).toBe('postgres · 41 MB · held by a running session')
  expect(serviceVolumeLine({ ...volume, mounted: false, used_ms: now }, now)).toBe(
    'postgres · 41 MB · last used just now · its entry no longer names this service',
  )
})

test('service data is reset only at the node, and never under a running session', () => {
  expect(resetRefusal({ live: false }, true)).toBeNull()
  expect(resetRefusal({ live: false }, false)).toBe('Service data is reset on the node itself.')
  expect(resetRefusal({ live: true }, true)).toBe('A running session holds it; reset it once that session ends.')
})
