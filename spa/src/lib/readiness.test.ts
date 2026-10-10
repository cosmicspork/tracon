import { expect, test } from 'bun:test'
import { allGaps, cannotWork, headline, ownGaps, remedy, summarize } from './readiness'
import type { RepoReadiness } from './types'

const gap = (key: string) => ({ key, message: `${key} is missing` })

const readiness: RepoReadiness = {
  channel: 'personal',
  repo: '/r',
  investigate: { purpose: 'investigate', ready: true, missing: [], notes: [] },
  verify: { purpose: 'verify', ready: false, missing: [gap('checks')], notes: [] },
  publish: {
    purpose: 'publish',
    ready: false,
    missing: [gap('checks'), gap('credential')],
    notes: [gap('brief')],
  },
}

test('each path reads as ready or as its first gap', () => {
  expect(summarize(readiness)).toEqual([
    { purpose: 'investigate', ready: true, line: 'ready' },
    { purpose: 'verify', ready: false, line: 'checks is missing' },
    { purpose: 'publish', ready: false, line: 'checks is missing (+1 more)' },
  ])
})

test('a later path names only what it adds', () => {
  expect(ownGaps(readiness, 'verify').map((g) => g.key)).toEqual(['checks'])
  expect(ownGaps(readiness, 'publish').map((g) => g.key)).toEqual(['credential'])
})

test('a ready repository has no headline', () => {
  const ready = (purpose: 'investigate' | 'verify' | 'publish') => ({ purpose, ready: true, missing: [], notes: [gap('image')] })
  expect(headline({ ...readiness, verify: ready('verify'), publish: ready('publish') })).toBe('')
})

test('the headline names the paths that cannot be taken', () => {
  expect(headline(readiness)).toBe('Not ready to verify or publish')
  const stuck = { purpose: 'investigate' as const, ready: false, missing: [gap('repo')], notes: [] }
  expect(headline({ ...readiness, investigate: stuck })).toBe('No session can start here')
})

test('each gap is listed once, under the first path it stops, with where it is closed', () => {
  expect(allGaps(readiness).map(({ purpose, gap }) => `${purpose}:${gap.key}`)).toEqual(['verify:checks', 'publish:credential'])
  expect(remedy('credential')?.href).toBe('/settings#connections')
  expect(remedy('remote')).toBeNull()
})

test('only a repository nothing can investigate stops a launch', () => {
  expect(cannotWork(null)).toBeNull()
  expect(cannotWork(readiness)).toBeNull()
  const nothing = { ...readiness, investigate: { purpose: 'investigate' as const, ready: false, missing: [gap('repo'), gap('launch')], notes: [] } }
  expect(cannotWork(nothing)).toBe('repo is missing (+1 more)')
})
