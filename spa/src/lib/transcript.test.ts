import { expect, test } from 'bun:test'

import { groupLog, groupSummary, type ToolEntry } from './log'
import {
  callKind,
  callTarget,
  callText,
  decodeOutput,
  firstErrorLine,
  formatSpan,
  outputTail,
  runFailures,
  runMs,
  runRunning,
  transcript,
  unwrapHarnessError,
} from './transcript'
import type { Event } from './types'

const ROOT = '/var/lib/tracon/worktrees/orbit-b8c7d0'
const T0 = 1_790_000_000_000

let seq = 0
function ev(kind: string, payload: Record<string, unknown>, at: number, ref: string | null = null): Event {
  seq += 1
  return { seq, node_id: 'n', session_id: 's', kind, ref_id: ref, payload, at_ms: T0 + at, mono_ms: seq }
}

/// A Claude Code call: its tool's name as the title, `other` as its kind, and
/// the policy record that says what it did.
function call(id: string, name: string, kind: string, input: Record<string, unknown>, at: number, result?: { ms: number; output?: string; failed?: boolean }): Event[] {
  const out = [
    ev('tool_call', { title: name, kind: 'other', raw_input: input }, at, id),
    ev('policy_allowed', { kind, tool_call_id: id, policy: 'allow' }, at + 10),
  ]
  if (result) {
    const payload = { status: result.failed ? 'failed' : 'completed', output: result.output === undefined ? null : JSON.stringify(result.output) }
    out.push(ev('tool_result', payload, at + result.ms, id))
  }
  return out
}

function run(events: Event[], tails?: Map<string, string>): ToolEntry[] {
  const log = groupLog(events, new Map(), tails)
  expect(log).toHaveLength(1)
  return log[0].tools!
}

test('what a call acted on comes out of its raw_input', () => {
  expect(callTarget({ command: 'just test', description: 'Run the tests' })).toBe('just test')
  expect(callTarget({ command: ['cargo', 'test', '-p', 'tracon-node'] })).toBe('cargo test -p tracon-node')
  expect(callTarget({ command: 'cd node\n&& cargo test' })).toBe('cd node …')
  expect(callTarget({ file_path: `${ROOT}/node/src/log.rs`, offset: 10 }, ROOT)).toBe('node/src/log.rs')
  expect(callTarget({ filePath: `${ROOT}/spa/src/x.ts` }, ROOT)).toBe('spa/src/x.ts')
  expect(callTarget({ file_path: '/etc/hosts' }, ROOT)).toBe('/etc/hosts')
  expect(callTarget({ pattern: 'fn ceiling', path: `${ROOT}/node/src` }, ROOT)).toBe('"fn ceiling" in node/src')
  expect(callTarget({ pattern: '**/*.ts' })).toBe('"**/*.ts"')
  expect(callTarget({ url: 'https://example.com/a' })).toBe('https://example.com/a')
  expect(callTarget(null)).toBe('')
  expect(callTarget('a string')).toBe('')
})

test('the tool name is not repeated when the title already names its target', () => {
  const [entry] = run(call('a', 'Bash', 'execute', { command: 'just test' }, 0))
  expect(callText(entry)).toBe('Bash just test')
  const [titled] = run([ev('tool_call', { title: 'read node/src/sidecars.rs', kind: 'read', raw_input: { filePath: 'node/src/sidecars.rs' } }, 0, 'b')])
  expect(callText(titled)).toBe('read node/src/sidecars.rs')
})

test('a call no policy record labelled gets its kind from its tool name', () => {
  const [bash] = run([ev('tool_call', { title: 'bash', kind: 'other', raw_input: { command: 'ls' } }, 0, 'c')])
  expect(callKind(bash)).toBe('execute')
  const [grep] = run([ev('tool_call', { title: 'Grep', kind: 'other' }, 0, 'd')])
  expect(callKind(grep)).toBe('search')
  const [mystery] = run([ev('tool_call', { title: 'mcp__forge__comment', kind: 'other' }, 0, 'e')])
  expect(callKind(mystery)).toBe('other')
  const [labelled] = run(call('f', 'Read', 'read', {}, 0))
  expect(callKind(labelled)).toBe('read')
})

test('a stored output reads back as the text the tool printed, even cut short', () => {
  expect(decodeOutput(JSON.stringify('232 pass\n0 fail'))).toBe('232 pass\n0 fail')
  expect(decodeOutput(JSON.stringify({ ok: true }))).toBe('{\n  "ok": true\n}')
  expect(decodeOutput(JSON.stringify('line one\n"quoted"\tand é').slice(0, 26))).toBe('line one\n"quoted"\tand')
  expect(decodeOutput('null')).toBe('')
  expect(decodeOutput(null)).toBe('')
})

test('a failure names its first error line, not the first line it printed', () => {
  const out = '   Compiling tracon-node v0.30.0\nerror[E0425]: cannot find value `window`\n --> model.rs:212\n\nerror: could not compile'
  expect(firstErrorLine(out)).toBe('error[E0425]: cannot find value `window`')
  expect(firstErrorLine('\n  exit status 2\n')).toBe('exit status 2')
  expect(firstErrorLine('')).toBe('')
  expect(firstErrorLine(`error: ${'x'.repeat(300)}`)).toHaveLength(160)
})

test('a running call shows the last lines of its output', () => {
  expect(outputTail('a\nb\n\nc\n')).toBe('b\nc')
  expect(outputTail('only')).toBe('only')
  expect(outputTail('')).toBe('')
})

test('durations are precise for quick calls and short for long ones', () => {
  expect(formatSpan(420)).toBe('0.4s')
  expect(formatSpan(4_250)).toBe('4.3s')
  expect(formatSpan(48_200)).toBe('48s')
  expect(formatSpan(200_000)).toBe('3m 20s')
  expect(formatSpan(120_000)).toBe('2m')
  expect(formatSpan(3_900_000)).toBe('1h 5m')
  expect(formatSpan(-5)).toBe('0.0s')
})

test('consecutive reads share one head; a read between other calls stands alone', () => {
  const tools = run([
    ...call('r1', 'Read', 'read', { file_path: `${ROOT}/node/src/gateway/model.rs` }, 0, { ms: 300, output: 'a' }),
    ...call('r2', 'Read', 'read', { file_path: `${ROOT}/node/src/metrics.rs` }, 1000, { ms: 200, output: 'b' }),
    ...call('r3', 'Read', 'read', { file_path: `${ROOT}/node/src/gateway/mod.rs` }, 2000, { ms: 500, output: 'c' }),
    ...call('e1', 'Edit', 'edit', { file_path: `${ROOT}/node/src/metrics.rs` }, 3000, { ms: 600 }),
    ...call('r4', 'Read', 'read', { file_path: `${ROOT}/node/src/lib.rs` }, 4000, { ms: 100 }),
  ])
  const rows = transcript(tools, ROOT)
  expect(rows.map((r) => r.kind)).toEqual(['reads', 'call', 'call'])
  const head = rows[0]
  if (head.kind !== 'reads') throw new Error('expected reads')
  expect(head.text).toBe('Read 3 files · model.rs, metrics.rs, mod.rs')
  expect(head.calls.map((c) => c.text)).toEqual(['Read node/src/gateway/model.rs', 'Read node/src/metrics.rs', 'Read node/src/gateway/mod.rs'])
  expect(head.calls.map((c) => c.output)).toEqual(['a', 'b', 'c'])
  expect(head.ms).toBe(2500)
  expect(head.state).toBe('done')
  expect(rows[2].kind === 'call' && rows[2].call.text).toBe('Read node/src/lib.rs')
})

test('a failed call carries its first error line and the run names it folded', () => {
  const tools = run([
    ...call('r1', 'Read', 'read', { file_path: `${ROOT}/a.rs` }, 0, { ms: 100 }),
    ...call('b1', 'Bash', 'execute', { command: 'cargo build' }, 1000, { ms: 48_200, failed: true, output: 'Compiling\nerror[E0425]: no `window`\n' }),
  ])
  const [read, build] = transcript(tools, ROOT)
  expect(read.kind === 'call' && read.call.error).toBe('')
  if (build.kind !== 'call') throw new Error('expected a call')
  expect(build.call.state).toBe('failed')
  expect(build.call.error).toBe('error[E0425]: no `window`')
  expect(build.call.ms).toBe(48_200)
  expect(runFailures(tools, ROOT)).toEqual([{ text: 'Bash cargo build', error: 'error[E0425]: no `window`' }])
  expect(groupSummary(tools)).toBe('Read 1 file, ran 1 shell command · 1 failed')
  expect(runMs(tools)).toBe(49_200)
})

test('a running call says how long it has been going and the tail of its output', () => {
  const events = [
    ...call('e1', 'Edit', 'edit', { file_path: `${ROOT}/a.rs` }, 0, { ms: 500 }),
    ...call('t1', 'Bash', 'execute', { command: 'cargo test' }, 2000),
  ]
  const tails = new Map([['t1', JSON.stringify('running 12 tests\ntest a ... ok\ntest b ... ok\n')]])
  const tools = run(events, tails)
  const now = T0 + 11_500
  const rows = transcript(tools, ROOT, now)
  const last = rows[1]
  if (last.kind !== 'call') throw new Error('expected a call')
  expect(last.call.state).toBe('running')
  expect(last.call.ms).toBe(9_500)
  expect(last.call.tail).toBe('test a ... ok\ntest b ... ok')
  expect(runMs(tools, now)).toBe(11_500)
  // Finished calls never carry a tail, whatever the stream last said.
  expect(rows[0].kind === 'call' && rows[0].call.tail).toBe('')
})

test('a run with no times says nothing about how long it took', () => {
  const tools = run([{ seq: 1, node_id: 'n', session_id: 's', kind: 'tool_call', ref_id: 'x', payload: { title: 'Read' }, at_ms: 0, mono_ms: 0 }])
  expect(runMs(tools, 0)).toBeNull()
  expect(transcript(tools)[0].kind === 'call' && (transcript(tools)[0] as { call: { ms: number | null } }).call.ms).toBeNull()
})

test("Claude Code's error wrapper is taken off a failed call's output", () => {
  expect(unwrapHarnessError('<tool_use_error>No changes to make: old_string and new_string are exactly the same.</tool_use_error>')).toBe(
    'No changes to make: old_string and new_string are exactly the same.',
  )
  expect(unwrapHarnessError('\n<tool_use_error>Error: No such tool available: x</tool_use_error>\n')).toBe('Error: No such tool available: x')
  // The tags inside other output are what the tool printed, and stay.
  expect(unwrapHarnessError('grep found <tool_use_error> in log.ts')).toBe('grep found <tool_use_error> in log.ts')
  expect(unwrapHarnessError('')).toBe('')

  const tools = run(
    call('e', 'Edit', 'edit', { file_path: `${ROOT}/spa/src/x.ts` }, 0, {
      ms: 20,
      failed: true,
      output: '<tool_use_error>No changes to make: old_string and new_string are exactly the same.</tool_use_error>',
    }),
  )
  const [row] = transcript(tools, ROOT)
  expect(row.kind === 'call' && row.call.output).toBe('No changes to make: old_string and new_string are exactly the same.')
  expect(runFailures(tools, ROOT)).toEqual([
    { text: 'Edit spa/src/x.ts', error: 'No changes to make: old_string and new_string are exactly the same.' },
  ])
})

test('a running run names its calls still going, and none once they land', () => {
  const going = run([
    ...call('a', 'Bash', 'execute', { command: 'ls' }, 0, { ms: 100, output: 'x' }),
    ...call('b', 'Bash', 'execute', { command: 'just check' }, 200),
  ])
  const rows = runRunning(going, ROOT, T0 + 5_200)
  expect(rows.map((r) => [r.text, r.state, r.ms])).toEqual([['Bash just check', 'running', 5_000]])

  const landed = run([
    ...call('a', 'Bash', 'execute', { command: 'ls' }, 0, { ms: 100, output: 'x' }),
    ...call('b', 'Bash', 'execute', { command: 'just check' }, 200, { ms: 300 }),
  ])
  expect(runRunning(landed, ROOT)).toEqual([])
})
