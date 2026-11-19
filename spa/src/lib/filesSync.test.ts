import { expect, test } from 'bun:test'
import { STALE_AFTER_MS, filesSyncNote, logRoot } from './filesSync'
import type { FilesSync } from './types'

const now = 10_000_000
const live = (o: Partial<FilesSync> = {}): FilesSync => ({
  last_ok_ms: now - 40_000,
  failures: 0,
  last_error: null,
  in_flight: false,
  live: true,
  ...o,
})

test('a running sandbox says how old the node copy is', () => {
  expect(filesSyncNote(live(), now, true)).toMatchObject({ text: 'node copy 40s old', warn: false })
  expect(filesSyncNote(live({ in_flight: true }), now, true)?.text).toBe('node copy 40s old, syncing')
})

test('it warns past the threshold only while a turn runs', () => {
  const justUnder = live({ last_ok_ms: now - STALE_AFTER_MS })
  expect(filesSyncNote(justUnder, now, true)?.warn).toBe(false)
  const over = live({ last_ok_ms: now - STALE_AFTER_MS - 1000 })
  expect(filesSyncNote(over, now, true)).toMatchObject({ text: 'node copy 5m old', warn: true })
  // Between turns nothing changes in the sandbox: an old clean copy is current.
  expect(filesSyncNote(over, now, false)?.warn).toBe(false)
})

test('failing syncs are said with their reason', () => {
  const failing = live({
    last_ok_ms: now - 12 * 60_000,
    failures: 3,
    last_error: 'taking the trees back took longer than 120s',
  })
  expect(filesSyncNote(failing, now, true)).toMatchObject({
    text: 'node copy 12m old, last 3 syncs failed: taking the trees back took longer than 120s',
    warn: true,
  })
})

test('an ended sandbox whose last take-back failed keeps saying so', () => {
  const ended = live({ live: false, failures: 1, last_error: 'the sandbox is gone' })
  expect(filesSyncNote(ended, now, false)).toMatchObject({
    text: 'the sandbox ended with changes the node may not have',
    warn: true,
  })
  expect(filesSyncNote(live({ live: false }), now, false)?.warn).toBe(false)
})

test('nothing is said for a session nothing is taken back from', () => {
  expect(filesSyncNote(null, now, true)).toBeNull()
  expect(filesSyncNote(undefined, now, true)).toBeNull()
})

test('the log strips only a root its tool paths start with', () => {
  expect(logRoot({ harness_id: 'claude', worktree_path: '/state/tracon/workspaces/x' })).toBe('/work')
  expect(logRoot({ harness_id: 'external', worktree_path: '/home/op/src/orbit-wt' })).toBe('/home/op/src/orbit-wt')
})
