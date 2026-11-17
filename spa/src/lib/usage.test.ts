import { expect, test } from 'bun:test'
import type { SessionUsage } from './types'
import { runtimeNote, usageNote } from './usage'

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

test('a session that ran on DigitalOcean shows its runtime cost beside the model cost', () => {
  const sandbox = {
    sandbox_id: 'do-1',
    provider: 'digitalocean',
    name: null,
    kind: 'session',
    session_id: 's1',
    channel: 'work',
    size: 'small',
    price_per_hour_usd: 0.1,
    started_ms: 0,
    ended_ms: 1,
    end_reason: 'destroyed',
  }
  expect(runtimeNote(null)).toBeNull()
  expect(runtimeNote({ cost_usd: null, hours: 0, running: false, sandboxes: [] })).toBeNull()
  const note = runtimeNote({ cost_usd: 0.25, hours: 2.5, running: true, sandboxes: [sandbox] })
  expect(note?.text).toBe('runtime $0.25')
  expect(note?.title).toContain('2.5h at small, still running')
  expect(runtimeNote({ cost_usd: null, hours: 0.5, running: false, sandboxes: [sandbox] })?.text).toBe('runtime unpriced')
})
