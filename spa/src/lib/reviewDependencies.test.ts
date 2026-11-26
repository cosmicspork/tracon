import { expect, test } from 'bun:test'
import { afterLine, anywayReason, unmet, waitsOnLine } from './reviewDependencies'
import { nameOf } from './names'
import { unavailable } from './verdicts'
import type { ReviewDependencies } from './types'

const deps = (...after: [string, string, boolean][]): ReviewDependencies => ({
  after: after.map(([review_id, says, satisfied]) => ({ review_id, title: 't', state: 'approved', says, satisfied })),
  before: [],
})

test('only dependencies not yet satisfied are waited on', () => {
  const d = deps(['a1', 'merged', true], ['b2', 'waiting on merge', false])
  expect(unmet(d).map((x) => x.review_id)).toEqual(['b2'])
  expect(waitsOnLine(d)).toBe(`waits on ${nameOf('review', 'b2')} · waiting on merge`)
  expect(waitsOnLine(deps(['a1', 'merged', true]))).toBeNull()
  expect(waitsOnLine(null)).toBeNull()
})

test('approve is blocked while a dependency waits, and an approval anyway says why', () => {
  const d = deps(['b2', 'waiting on review', false])
  const base = { busy: false, publishing: false, stale: [], reason: '' }
  const line = waitsOnLine(d)
  expect(unavailable('approve', { ...base, waitsOn: line })).toBe(`waits on ${nameOf('review', 'b2')} · waiting on review`)
  // Approve anyway lifts the block; the approval then carries the reason.
  expect(unavailable('approve', { ...base, waitsOn: null })).toBeNull()
  expect(anywayReason(d)).toBe(`Approved anyway: ${line}`)
  expect(anywayReason(deps(['b2', 'merged', true]))).toBeUndefined()
  // Stale files still come first: they are about this review's own bytes.
  expect(unavailable('approve', { ...base, stale: ['a.txt'], waitsOn: line })).toContain('changed since submit')
})

test('a home row names what it publishes after', () => {
  expect(afterLine(['a1'])).toBe(`after ${nameOf('review', 'a1')}`)
  expect(afterLine([])).toBeNull()
  expect(afterLine(undefined)).toBeNull()
})
