import { expect, test } from 'bun:test'
import type { SessionUsage } from './types'
import { usageNote } from './usage'

const base: SessionUsage = {
  state: 'reconciled',
  gateway_tokens: 1200,
  harness_tokens: 1200,
  charged_tokens: 1200,
  unmetered_turns: 0,
  mismatched_turns: 0,
  turns: [],
}

test('nothing is said before a turn has settled', () => {
  expect(usageNote(null)).toBe(null)
  expect(usageNote({ ...base, state: null })).toBe(null)
})

test('a harness that reports no requests says nothing about them', () => {
  const note = usageNote({ ...base, requests_matched: null, requests_unmatched: null })
  expect(note?.warn).toBe(false)
  expect(note?.text).not.toContain('requests')
  expect(usageNote(base)?.text).not.toContain('requests')
})

test('requests matched one by one are counted against all of them', () => {
  expect(usageNote({ ...base, requests_matched: 4, requests_unmatched: 0 })?.text).toEndWith(
    '· reconciled · 4 of 4 requests matched',
  )
  const note = usageNote({
    ...base,
    state: 'mismatch',
    mismatched_turns: 1,
    requests_matched: 3,
    requests_unmatched: 1,
  })
  expect(note?.warn).toBe(true)
  expect(note?.text).toEndWith('· 1 mismatched · 3 of 4 requests matched')
})
