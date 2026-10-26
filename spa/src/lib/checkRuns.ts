import type { CandidateCheckRun } from './types'

/** How a run reads at a glance: a green dot, a red one, or still going. */
export type RunTone = 'ok' | 'bad' | 'running'

/**
 * What a run says about the candidate. A reused run points at the one that
 * actually executed, so its source's outcome is the one it carries; one
 * whose source is unknown says nothing passed.
 */
export function effectiveOutcome(run: CandidateCheckRun): string {
  return run.outcome === 'reused' ? (run.source_outcome ?? 'interrupted') : run.outcome
}

export function runTone(run: CandidateCheckRun): RunTone {
  const outcome = effectiveOutcome(run)
  if (outcome === 'passed') return 'ok'
  if (outcome === 'running') return 'running'
  return 'bad'
}

/** The outcome in words, as a person would say it. */
export function outcomeLabel(run: CandidateCheckRun): string {
  const word = (o: string) => o.replace(/_/g, ' ')
  return run.outcome === 'reused' ? `reused · ${word(run.source_outcome ?? 'unknown source')}` : word(run.outcome)
}

/** How long a run took: "94s", "3m 32s", or "running" while it is. */
export function runDuration(run: CandidateCheckRun): string {
  if (run.outcome === 'running') return 'running'
  if (run.duration_ms == null) return run.outcome === 'reused' ? 'reused' : 'unfinished'
  const s = Math.round(run.duration_ms / 1000)
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`
}

/** "4 checks passed", "1 of 4 failed", "1 of 3 running". */
export function runsSummary(runs: CandidateCheckRun[]): string {
  const n = runs.length
  const bad = runs.filter((r) => runTone(r) === 'bad').length
  const running = runs.filter((r) => runTone(r) === 'running').length
  if (bad) return `${bad} of ${n} failed${running ? ` · ${running} running` : ''}`
  if (running) return `${running} of ${n} running`
  return n === 1 ? '1 check passed' : `${n} checks passed`
}

/** The lines of a failed run's whole output that say what failed, which its log tail may not hold. */
export function failuresOf(run: CandidateCheckRun): string {
  try {
    const failures = (JSON.parse(run.metadata_json) as { failures?: unknown }).failures
    return typeof failures === 'string' ? failures : ''
  } catch {
    return ''
  }
}

/** One row of a run's inputs: a label and its value, or the values of a list. */
export interface InputRow {
  label: string
  values: string[]
}

/** The inputs a node records, in the order a reader asks about them. */
const KNOWN: [key: string, label: string][] = [
  ['egress', 'Egress hosts'],
  ['prepare', 'Prepare commands'],
  ['runtime', 'Runtime'],
  ['timeout_secs', 'Timeout'],
  ['env', 'Env'],
  ['dependency_inputs', 'Dependency inputs'],
]

function values(key: string, v: unknown): string[] {
  if (v === null || v === undefined) return []
  if (key === 'timeout_secs' && typeof v === 'number') return [`${v}s`]
  if (Array.isArray(v)) {
    // `env` is recorded as [name, value] pairs.
    return v.map((x) => (Array.isArray(x) && x.length === 2 ? `${x[0]}=${x[1]}` : typeof x === 'string' ? x : JSON.stringify(x)))
  }
  if (typeof v === 'object') return Object.entries(v as Record<string, unknown>).map(([k, x]) => `${k}=${typeof x === 'string' ? x : JSON.stringify(x)}`)
  return [String(v)]
}

/**
 * A run's recorded inputs as rows to read, the known ones first and anything
 * else after under its own key. Null when the run recorded none; a value the
 * node wrote that is not JSON is shown as it is.
 */
export function inputRows(run: CandidateCheckRun): InputRow[] | null {
  if (!run.inputs_json) return null
  let inputs: unknown
  try {
    inputs = JSON.parse(run.inputs_json)
  } catch {
    return [{ label: 'Inputs', values: [run.inputs_json] }]
  }
  if (inputs === null || typeof inputs !== 'object' || Array.isArray(inputs)) {
    return [{ label: 'Inputs', values: [run.inputs_json] }]
  }
  const all = inputs as Record<string, unknown>
  const known = new Set(KNOWN.map(([k]) => k))
  const rows: InputRow[] = []
  for (const [key, label] of KNOWN) if (key in all) rows.push({ label, values: values(key, all[key]) })
  for (const [key, v] of Object.entries(all)) if (!known.has(key)) rows.push({ label: key.replace(/_/g, ' '), values: values(key, v) })
  return rows
}
