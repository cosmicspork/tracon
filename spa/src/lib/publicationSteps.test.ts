import { describe, expect, test } from 'bun:test'

import { changeNumber, fromView, latestProgress, stepRows } from './publicationSteps'
import type { PublicationProgress, PublicationView } from './types'

const PLAN = ['verifying', 'inspecting_forge', 'pushing', 'confirming_push', 'opening_change', 'published'] as const

function frame(over: Partial<PublicationProgress> = {}): PublicationProgress {
  return {
    id: 'p1',
    review_id: 'r1',
    node_id: 'n1',
    provider: 'github',
    branch: 'feat/x',
    outcome: 'in_progress',
    step: 'pushing',
    step_started_ms: 10_000,
    steps: [...PLAN],
    reason: null,
    url: null,
    ...over,
  }
}

function view(over: Partial<PublicationView> = {}): PublicationView {
  return {
    id: 'p1',
    outcome: 'in_progress',
    note: null,
    url: null,
    attempts: 1,
    updated_ms: 1,
    provider: 'github',
    project: 'owner/name',
    branch: 'feat/x',
    remedy: '',
    recoverable: false,
    refusal: null,
    step: 'confirming_push',
    step_started_ms: 20_000,
    steps: [...PLAN],
    ...over,
  }
}

describe('stepRows', () => {
  test('steps before the current one are done, it runs, and the rest wait', () => {
    const rows = stepRows(frame(), 10_500)
    expect(rows.map((r) => [r.step, r.state])).toEqual([
      ['verifying', 'done'],
      ['inspecting_forge', 'done'],
      ['pushing', 'running'],
      ['confirming_push', 'pending'],
      ['opening_change', 'pending'],
      ['published', 'pending'],
    ])
    expect(rows[2]!.label).toBe('Pushing feat/x')
    expect(rows[4]!.label).toBe('Opening the pull request')
    // Not slow yet: no time shown.
    expect(rows[2]!.elapsed).toBeNull()
  })

  test('a slow step shows how long it has taken', () => {
    expect(stepRows(frame(), 17_400)[2]!.elapsed).toBe('7s')
  })

  test('a failed attempt marks the step it stopped at with the reason', () => {
    const rows = stepRows(frame({ outcome: 'failed', step: 'opening_change', reason: 'HTTP 422' }), 0)
    const failed = rows.find((r) => r.state === 'failed')!
    expect(failed.step).toBe('opening_change')
    expect(failed.reason).toBe('HTTP 422')
    expect(rows.filter((r) => r.reason).length).toBe(1)
    expect(rows.at(-1)!.state).toBe('pending')
  })

  test('an unknown outcome is not shown as a failure', () => {
    const rows = stepRows(frame({ outcome: 'uncertain', step: 'pushing', reason: 'verify on the forge' }), 0)
    expect(rows[2]!.state).toBe('unknown')
  })

  test('published, every step is done and the last links the change', () => {
    const rows = stepRows(frame({ outcome: 'published', step: 'published', url: 'https://github.test/pull/1234' }), 0)
    expect(rows.every((r) => r.state === 'done')).toBe(true)
    expect(rows.at(-1)).toMatchObject({ label: 'Published: #1234', url: 'https://github.test/pull/1234' })
  })

  test('a merge request is called one', () => {
    expect(stepRows(frame({ provider: 'gitlab', step: 'opening_change' }), 0)[4]!.label).toBe('Opening the merge request')
  })
})

describe('the step shown', () => {
  test('a loaded in-progress record is read as its frame', () => {
    const loaded = fromView(view(), 'r1')!
    expect(loaded).toMatchObject({ review_id: 'r1', step: 'confirming_push', outcome: 'in_progress', reason: null })
    expect(stepRows(loaded, 20_000).find((r) => r.state === 'running')!.step).toBe('confirming_push')
  })

  test('a record from before steps were kept shows none', () => {
    expect(fromView(view({ step: undefined, steps: undefined }), 'r1')).toBeNull()
  })

  test('the newer of the stream and the record wins', () => {
    const loaded = fromView(view(), 'r1')
    // The record was read after this frame.
    expect(latestProgress(frame(), loaded)!.step).toBe('confirming_push')
    // The next frame overtakes the record.
    const next = frame({ step: 'opening_change', step_started_ms: 30_000 })
    expect(latestProgress(next, loaded)!.step).toBe('opening_change')
    // At the same step, the settled one is newer.
    const failed = fromView(view({ outcome: 'failed', note: 'refused' }), 'r1')
    expect(latestProgress(frame({ step: 'confirming_push', step_started_ms: 20_000 }), failed)!.outcome).toBe('failed')
    expect(latestProgress(null, loaded)).toBe(loaded)
    expect(latestProgress(next, null)).toBe(next)
  })
})

test('changeNumber reads the number off a change address', () => {
  expect(changeNumber('https://github.com/o/n/pull/7')).toBe('7')
  expect(changeNumber('https://gitlab.com/o/n/-/merge_requests/12/')).toBe('12')
  expect(changeNumber('https://github.com/o/n/tree/main')).toBeNull()
  expect(changeNumber(null)).toBeNull()
})
