import { describe, expect, test } from 'bun:test'
import { inputRows, outcomeLabel, runDuration, runTone, runsSummary } from './checkRuns'
import type { CandidateCheckRun } from './types'

const run = (over: Partial<CandidateCheckRun> = {}): CandidateCheckRun => ({
  id: 'run-1',
  candidate_id: 'cand-1',
  session_id: 's-1',
  command: 'just check',
  definition_json: '{}',
  definition_hash: null,
  execution_image: 'ghcr.io/o/ci@sha256:4e9a',
  inputs_json: null,
  reuse_key: null,
  outcome: 'passed',
  source_outcome: null,
  exit_code: 0,
  log: '',
  duration_ms: 94000,
  started_ms: 0,
  finished_ms: 94000,
  rerun_of: null,
  reused_from_id: null,
  metadata_json: '{}',
  ...over,
})

describe('check runs on the review', () => {
  test('a reused run reads as the run it points at', () => {
    expect(runTone(run({ outcome: 'reused', source_outcome: 'passed' }))).toBe('ok')
    expect(runTone(run({ outcome: 'reused', source_outcome: 'failed' }))).toBe('bad')
    expect(runTone(run({ outcome: 'reused', source_outcome: null }))).toBe('bad')
    expect(outcomeLabel(run({ outcome: 'reused', source_outcome: 'passed' }))).toBe('reused · passed')
    expect(runTone(run({ outcome: 'not_runnable' }))).toBe('bad')
    expect(outcomeLabel(run({ outcome: 'not_runnable' }))).toBe('not runnable')
    expect(runTone(run({ outcome: 'running' }))).toBe('running')
  })

  test('the summary says all passed, or how many of them failed', () => {
    expect(runsSummary([run(), run(), run(), run()])).toBe('4 checks passed')
    expect(runsSummary([run()])).toBe('1 check passed')
    expect(runsSummary([run(), run({ outcome: 'failed' }), run(), run()])).toBe('1 of 4 failed')
    expect(runsSummary([run(), run({ outcome: 'running' })])).toBe('1 of 2 running')
  })

  test('a duration reads in seconds, then minutes and seconds', () => {
    expect(runDuration(run({ duration_ms: 94000 }))).toBe('1m 34s')
    expect(runDuration(run({ duration_ms: 41000 }))).toBe('41s')
    expect(runDuration(run({ outcome: 'running', duration_ms: null }))).toBe('running')
    expect(runDuration(run({ outcome: 'reused', duration_ms: null }))).toBe('reused')
  })

  test('inputs read as rows, the known ones first and in words', () => {
    const rows = inputRows(
      run({
        inputs_json: JSON.stringify({
          runtime: 'podman',
          timeout_secs: 900,
          dependency_inputs: { lockfile: 'Cargo.lock@91c4e2' },
          prepare: ['cargo fetch'],
          egress: ['crates.io', 'static.crates.io'],
          env: [['CARGO_NET_OFFLINE', 'true']],
          toolchain: '1.91.0',
        }),
      }),
    )
    expect(rows).toEqual([
      { label: 'Egress hosts', values: ['crates.io', 'static.crates.io'] },
      { label: 'Prepare commands', values: ['cargo fetch'] },
      { label: 'Runtime', values: ['podman'] },
      { label: 'Timeout', values: ['900s'] },
      { label: 'Env', values: ['CARGO_NET_OFFLINE=true'] },
      { label: 'Dependency inputs', values: ['lockfile=Cargo.lock@91c4e2'] },
      { label: 'toolchain', values: ['1.91.0'] },
    ])
  })

  test('inputs never recorded say so, and ones that are not JSON are shown as they are', () => {
    expect(inputRows(run({ inputs_json: null }))).toBeNull()
    expect(inputRows(run({ inputs_json: 'tree=abc' }))).toEqual([{ label: 'Inputs', values: ['tree=abc'] }])
  })
})
