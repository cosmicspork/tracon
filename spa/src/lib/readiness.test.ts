import { expect, test } from 'bun:test'
import { ownGaps, summarize } from './readiness'
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
