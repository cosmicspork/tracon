import { describe, expect, test } from 'bun:test'
import type { ReviewerView } from './types'
import {
  basisLabel,
  checksState,
  composerLabel,
  publishedChange,
  rereadDelay,
  rereadReviewer,
  reviewerControl,
  startedHow,
  unavailable,
} from './verdicts'

const ready = { busy: false, publishing: false, stale: [] as string[], reason: '' }

describe('verdict availability', () => {
  test('approve needs nothing but a current, settled review', () => {
    expect(unavailable('approve', ready)).toBeNull()
    expect(unavailable('approve', { ...ready, stale: ['a.txt'] })).toContain('a.txt changed since submit')
    expect(unavailable('approve', { ...ready, stale: ['a', 'b'] })).toContain('2 files')
    expect(unavailable('approve', { ...ready, publishing: true })).toContain('reconcile')
  })

  test('approve says why when this node cannot publish it', () => {
    expect(unavailable('approve', { ...ready, readiness: { ready: true } })).toBeNull()
    const problem = 'no GitHub token (GITHUB_TOKEN) is stored on this node'
    expect(unavailable('approve', { ...ready, readiness: { ready: false, problem } })).toBe(
      `cannot publish from this node yet: ${problem}`,
    )
    expect(unavailable('approve', { ...ready, readiness: { ready: false } })).toContain('cannot publish')
    // Sending it back needs no token.
    expect(unavailable('revise', { ...ready, reason: 'x', readiness: { ready: false } })).toBeNull()
    // A stale review is named first: binding a token would not make it approvable.
    expect(unavailable('approve', { ...ready, stale: ['a'], readiness: { ready: false } })).toContain('changed since submit')
  })

  test('a verdict that sends a reason says it needs one', () => {
    expect(unavailable('revise', ready)).toBe('say what should change')
    expect(unavailable('reject', { ...ready, reason: '   ' })).toBe('say why you are rejecting it')
    expect(unavailable('revise', { ...ready, reason: 'rename it' })).toBeNull()
    // A stale review can still be sent back: that is how it gets resubmitted.
    expect(unavailable('revise', { ...ready, reason: 'x', stale: ['a'] })).toBeNull()
  })

  test('the composer is labelled for what it sends', () => {
    expect(composerLabel('reject', 0)).toContain('rejecting')
    expect(composerLabel('revise', 2)).toContain('2 files')
    expect(composerLabel('revise', 0)).toBe('What should change? The agent reads this.')
  })

  test('the published change is read from the forge URL', () => {
    expect(publishedChange('https://github.com/o/r/pull/418')).toEqual({ url: 'https://github.com/o/r/pull/418', number: 418 })
    expect(publishedChange('https://gitlab.example/g/p/-/merge_requests/7#note_1')?.number).toBe(7)
    expect(publishedChange('https://forge.example/o/r/compare/main...x')).toEqual({ url: 'https://forge.example/o/r/compare/main...x', number: null })
    expect(publishedChange('pushed abc123')).toBeNull()
    expect(publishedChange(null)).toBeNull()
    expect(publishedChange('')).toBeNull()
  })
})

const view = (over: Partial<ReviewerView> = {}): ReviewerView => ({
  state: 'none',
  model: 'm/reviewer',
  trigger: 'manual',
  source: 'session',
  bound: true,
  reason: null,
  revision: 1,
  ...over,
})

describe('the reviewer control', () => {
  test('with no review model there is nothing to start', () => {
    expect(reviewerControl('new', view({ bound: false, model: null }))).toEqual({ kind: 'no_model' })
  })

  test('a manual review never run on this revision offers to start one', () => {
    const reason = 'manual review: start a reviewer from the review page'
    expect(reviewerControl('new', view({ reason }))).toEqual({ kind: 'start', note: reason })
  })

  test('one starting or reading it offers nothing, and says since when', () => {
    expect(reviewerControl('new', view({ state: 'running', started_ms: 5 }))).toEqual({ kind: 'running', since: 5 })
    expect(reviewerControl('new', view({ state: 'starting' }))).toEqual({ kind: 'running', since: null })
    // The node is starting one it has not recorded yet.
    expect(reviewerControl('revising', view(), true)).toEqual({ kind: 'running', since: null })
  })

  test('a verdict on this revision offers to review again', () => {
    expect(reviewerControl('claimed', view({ state: 'done', verdict_revision: 1 }))).toEqual({ kind: 'again' })
  })

  test('a reviewer that failed or ran out of budget says why and offers a retry', () => {
    const reason = 'the reviewer used its token budget before giving a verdict'
    expect(reviewerControl('new', view({ state: 'failed', reason }))).toEqual({ kind: 'retry', reason })
    expect(reviewerControl('new', view({ state: 'failed' })).kind).toBe('retry')
  })

  test('a verdict on an earlier revision offers to review this one', () => {
    expect(reviewerControl('new', view({ revision: 2, verdict_revision: 1 }))).toEqual({ kind: 'stale', revision: 1 })
  })

  test('a decided or mirrored review offers nothing', () => {
    expect(reviewerControl('approved', view())).toEqual({ kind: 'hidden' })
    expect(reviewerControl('publishing', view())).toEqual({ kind: 'hidden' })
    expect(reviewerControl('new', null)).toEqual({ kind: 'hidden' })
  })

  test('the reviewed-by line says how the reviewer started', () => {
    expect(startedHow('submission')).toBe('started on submission')
    expect(startedHow('operator')).toBe('started by you')
    expect(startedHow(null)).toBeNull()
  })
})

describe('reading the reviewer again', () => {
  const running = view({ state: 'running', session_id: 'r2', started_ms: 5 })

  test('at once when the session it was reading with has ended', () => {
    const control = reviewerControl('new', running)
    expect(rereadReviewer(control, running, false, { id: 'r2', state: 'closed' })).toBe('now')
    expect(rereadReviewer(control, running, false, { id: 'r2', state: 'killed_budget' })).toBe('now')
  })

  test('only after a pause while it is still running', () => {
    const control = reviewerControl('new', running)
    expect(rereadReviewer(control, running, false, { id: 'r2', state: 'running' })).toBe('later')
    expect(rereadReviewer(control, running, false, undefined)).toBe('later')
  })

  test('only after a pause while a new one starts beside the last, ended one', () => {
    // Retry or Review again, or an external candidate's files still being
    // kept: the review still names the reviewer that ended.
    const done = view({ state: 'done', session_id: 'r1', verdict_revision: 1 })
    const failed = view({ state: 'failed', session_id: 'r1', reason: 'out of budget' })
    for (const v of [done, failed]) {
      const control = reviewerControl('new', v, true)
      expect(control.kind).toBe('running')
      expect(rereadReviewer(control, v, true, { id: 'r1', state: 'closed' })).toBe('later')
    }
    // Nor while the node says one is starting on a reviewer it still names.
    const starting = view({ state: 'running', session_id: 'r1' })
    expect(rereadReviewer(reviewerControl('new', starting, true), starting, true, { id: 'r1', state: 'failed' })).toBe(
      'later',
    )
  })

  test('not at all when nothing is starting or running', () => {
    const done = view({ state: 'done', session_id: 'r1' })
    expect(rereadReviewer(reviewerControl('new', done), done, false, { id: 'r1', state: 'closed' })).toBeNull()
  })
})

describe('a reviewer poll that fails', () => {
  test('is retried at once only when nothing has failed, then further off each time', () => {
    expect(rereadDelay('now', 0)).toBe(0)
    expect(rereadDelay('later', 0)).toBe(3000)
    expect(rereadDelay('now', 1)).toBe(3000)
    expect(rereadDelay('now', 2)).toBe(6000)
    expect(rereadDelay('later', 3)).toBe(12000)
    expect(rereadDelay('now', 20)).toBe(30000)
  })
})

describe('what a verdict rests on', () => {
  test('is said in the card’s words', () => {
    expect(basisLabel('ran_checks')).toBe('ran the checks')
    expect(basisLabel('cited_evidence')).toBe('cited evidence')
    expect(basisLabel('diff_only')).toBe('diff only')
  })

  test('is left unsaid for a verdict that did not say it', () => {
    expect(basisLabel(undefined)).toBeNull()
    expect(basisLabel(null)).toBeNull()
    expect(basisLabel('vibes')).toBeNull()
  })
})

describe('checksState', () => {
  test('says checks the node is still running as running, not as failed', () => {
    expect(checksState([{ ok: false, outcome: 'running' }])).toBe('running')
  })
  test('says whether every check passed', () => {
    expect(checksState([])).toBeNull()
    expect(checksState([{ ok: true }, { ok: true, outcome: 'passed' }])).toBe('passed')
    expect(checksState([{ ok: true }, { ok: false, outcome: 'failed' }])).toBe('failed')
    expect(checksState([{ ok: false, outcome: 'could_not_run' }])).toBe('failed')
  })
})
