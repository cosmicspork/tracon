import { describe, expect, test } from 'bun:test'
import { checksStatus, reviewerStatus, shortId } from './reviewStatus'
import type { CandidateCheckRun, CheckResult } from './types'

const run = (outcome: CandidateCheckRun['outcome'], id: string = outcome): CandidateCheckRun => ({
  id,
  candidate_id: 'cand-1',
  session_id: null,
  command: 'just check',
  definition_json: '{}',
  definition_hash: null,
  execution_image: null,
  inputs_json: null,
  reuse_key: null,
  outcome,
  source_outcome: null,
  exit_code: outcome === 'passed' ? 0 : 1,
  log: '',
  duration_ms: 1000,
  started_ms: 0,
  finished_ms: 1000,
  rerun_of: null,
  reused_from_id: null,
  metadata_json: '{}',
})

const recorded = (ok: boolean, outcome?: string): CheckResult => ({ command: 'just check', ok, exit: ok ? 0 : 1, tail: 'tail', ms: 1000, outcome })

describe('the review id chip', () => {
  test('is the first 8 hex digits, as agents write it', () => {
    expect(shortId('01a21416-7458-77b0-a822-408450a14cc8')).toBe('01a21416')
  })
})

describe('the checks item on the status line', () => {
  test('passed runs read as a tick and their count, and open to the runs', () => {
    expect(checksStatus([run('passed', 'a'), run('passed', 'b'), run('passed', 'c')], [])).toEqual({
      label: 'checks ✓ 3',
      tone: 'ok',
      expands: true,
    })
  })

  test('a failed run says how many of all failed', () => {
    expect(checksStatus([run('passed', 'a'), run('failed', 'b'), run('passed', 'c')], [])).toEqual({
      label: 'checks ✗ 1 of 3',
      tone: 'bad',
      expands: true,
    })
  })

  test('running runs say so, and that a waiting reviewer starts after', () => {
    expect(checksStatus([run('running')], []).label).toBe('checks running')
    expect(checksStatus([run('running')], [], true)).toEqual({
      label: 'checks running · reviewer starts after',
      tone: 'running',
      expands: true,
    })
  })

  test('no runs and no record reads as no checks, with nothing to open', () => {
    expect(checksStatus([], [])).toEqual({ label: 'no checks', tone: 'none', expands: false })
  })

  test("without runs, the review's own record is read the same way", () => {
    expect(checksStatus([], [recorded(true), recorded(true)]).label).toBe('checks ✓ 2')
    expect(checksStatus([], [recorded(true), recorded(false)]).label).toBe('checks ✗ 1 of 2')
    expect(checksStatus([], [recorded(false, 'running')]).tone).toBe('running')
    expect(checksStatus([], [recorded(false, 'could_not_run')])).toEqual({
      label: 'checks could not run',
      tone: 'bad',
      expands: true,
    })
  })

  test('the runs win over the older record', () => {
    expect(checksStatus([run('passed')], [recorded(false)]).label).toBe('checks ✓ 1')
  })
})

describe('the reviewer item on the status line', () => {
  const approve = { verdict: 'approve' as const }
  const changes = { verdict: 'request_changes' as const }

  test('a verdict reads as its chip, with Review again beside it', () => {
    expect(reviewerStatus({ kind: 'again' }, approve)).toEqual({ kind: 'verdict', label: 'approves', action: 'again', revision: null })
    expect(reviewerStatus({ kind: 'again' }, changes)).toMatchObject({ label: 'requests changes' })
  })

  test('a verdict on an earlier revision offers to review this one', () => {
    expect(reviewerStatus({ kind: 'stale', revision: 1 }, changes)).toEqual({
      kind: 'verdict',
      label: 'requests changes',
      action: 'stale',
      revision: 1,
    })
    expect(reviewerStatus({ kind: 'stale', revision: 1 }, null)).toEqual({ kind: 'stale', revision: 1 })
  })

  test('a reviewer at work reads as reviewing, for a peer too', () => {
    expect(reviewerStatus({ kind: 'running', since: 5 }, null)).toEqual({ kind: 'reviewing', since: 5 })
    expect(reviewerStatus({ kind: 'hidden' }, null, { peerReading: true })).toEqual({ kind: 'reviewing', since: null })
    // A peer's verdict, once in, is shown rather than "reviewing".
    expect(reviewerStatus({ kind: 'hidden' }, approve, { peerReading: true })).toMatchObject({ kind: 'verdict' })
  })

  test('the start and retry buttons carry why none ran, said once', () => {
    expect(reviewerStatus({ kind: 'start', note: 'manual review' }, null)).toEqual({ kind: 'start', note: 'manual review' })
    expect(reviewerStatus({ kind: 'retry', reason: 'timed out' }, null)).toEqual({ kind: 'retry', reason: 'timed out' })
  })

  test('no model bound reads as such, and an unreachable owner says why', () => {
    expect(reviewerStatus({ kind: 'no_model' }, null)).toEqual({ kind: 'no_model' })
    expect(reviewerStatus({ kind: 'hidden' }, null, { away: 'held by b, which cannot be read' })).toEqual({
      kind: 'away',
      note: 'held by b, which cannot be read',
    })
  })

  test('a decided review shows its verdict, and nothing when none was given', () => {
    expect(reviewerStatus({ kind: 'hidden' }, approve)).toEqual({ kind: 'verdict', label: 'approves', action: null, revision: null })
    expect(reviewerStatus({ kind: 'hidden' }, null)).toBeNull()
    expect(reviewerStatus(null, null)).toBeNull()
  })
})
