import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
  eventLine,
  fallbackLine,
  SILENT,
  groupLog,
  missingContext,
  missingLine,
  orientationContext,
  orientationLine,
  groupRunning,
  groupSummary,
  policyLine,
  providerErrorLine,
  repetitionHint,
  repetitionLine,
  sessionImage,
  usageMismatchLine,
  usageUnmeteredLine,
} from './log'
import type { Event } from './types'

let seq = 0
function ev(kind: string, ref_id: string | null = null, payload: Record<string, unknown> = {}): Event {
  seq += 1
  return { seq, node_id: 'n', session_id: 's', kind, ref_id, payload, at_ms: 0, mono_ms: seq }
}

test('consecutive tool calls fold into one group', () => {
  const log = groupLog([
    ev('user_prompt'),
    ev('tool_call', 'a', { kind: 'read' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'read' }),
    ev('tool_result', 'b', { status: 'completed' }),
    ev('message'),
  ])
  expect(log.map((l) => l.kind)).toEqual(['leaf', 'tools', 'leaf'])
  expect(log[1].tools!.length).toBe(2)
  expect(groupRunning(log[1].tools!)).toBe(false)
})

test('a group is running while a call lacks its result, and stops when it lands', () => {
  const running = groupLog([ev('tool_call', 'a', { kind: 'execute' })])
  expect(groupRunning(running[0].tools!)).toBe(true)
  const done = groupLog([ev('tool_call', 'a', { kind: 'execute' }), ev('tool_result', 'a', { status: 'completed' })])
  expect(groupRunning(done[0].tools!)).toBe(false)
})

test('a permission request breaks the group at its true position', () => {
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'execute' }),
    ev('permission_request', 'p1'),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'execute' }),
  ])
  expect(log.map((l) => l.kind)).toEqual(['tools', 'leaf', 'tools'])
  // The result still lands on its call even across the break.
  expect(log[0].tools![0].result).toBeDefined()
})

test('summary counts by kind and reports failures', () => {
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'read' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'read' }),
    ev('tool_result', 'b', { status: 'completed' }),
    ev('tool_call', 'c', { kind: 'execute' }),
    ev('tool_result', 'c', { status: 'failed' }),
  ])
  expect(groupSummary(log[0].tools!)).toBe('Read 2 files, ran 1 shell command · 1 failed')
})

test("a call's policy allow does not split the run, and names what the call did", () => {
  // Claude Code's order since every call got a policy record: call, the
  // harness's own allow, result. The asked path's allow has no call id.
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'other' }),
    ev('policy_allowed', null, { decided_by: 'harness', policy: 'allow', kind: 'read', tool_call_id: 'a' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'other' }),
    ev('policy_allowed', null, { kind: 'execute', rule: 'boundary-shell' }),
    ev('usage'),
    ev('tool_result', 'b', { status: 'failed' }),
    ev('tool_call', 'c', { kind: 'other' }),
    ev('policy_allowed', null, { kind: null, rule: 'node-tool' }),
    ev('tool_result', 'c', { status: 'completed' }),
  ])
  expect(log.map((l) => l.kind)).toEqual(['tools'])
  expect(log[0].tools!.length).toBe(3)
  expect(groupSummary(log[0].tools!)).toBe('Read 1 file, ran 1 shell command, called 1 tool · 1 failed')
})

test('a call the policy would have asked about stays in view', () => {
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'other' }),
    ev('policy_allowed', null, { decided_by: 'harness', policy: 'ask', rule: 'harness', title: 'Bash: git push' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'other' }),
  ])
  expect(log.map((l) => l.kind)).toEqual(['tools', 'leaf', 'tools'])
  expect(policyLine(log[1].event!)).toBe('ran without asking · the policy would have asked (harness) · Bash: git push')
})

test('a policy refusal reads as one', () => {
  expect(policyLine(ev('policy_denied', null, { title: 'Write: .git/config', reason: 'Git internals are the node’s.' }))).toBe(
    'refused by policy · Write: .git/config · Git internals are the node’s.',
  )
})

test('an orphan tool_result is kept as a leaf', () => {
  const log = groupLog([ev('tool_result', 'ghost', { status: 'completed' })])
  expect(log[0].kind).toBe('leaf')
})

test('a provider error reads as a retry in progress', () => {
  expect(providerErrorLine({ provider: 'anthropic', status: 429, message: 'Error', attempt: 2 })).toBe(
    'anthropic answered 429 · Error · harness retrying · attempt 2',
  )
  // The first attempt needs no number, and a harness notice may carry neither
  // a status nor a message.
  expect(providerErrorLine({ provider: 'anthropic', status: 429, attempt: 1 })).toBe(
    'anthropic answered 429 · harness retrying',
  )
  expect(providerErrorLine({})).toBe('the provider refused the call · harness retrying')
})

test('repetition reads as a signal to look at, not as a verdict', () => {
  expect(repetitionLine({ count: 3, title: 'run just test' })).toBe(
    'same call 3× in a row · run just test · recorded, not paused',
  )
  expect(repetitionLine({})).toBe('same call 0× in a row · the same tool call · recorded, not paused')
})

test('the start line says which image the harness is in only when it matters', () => {
  expect(sessionImage({ image: 'localhost/tracon-repo-app-claude@sha256:abc' })).toBe(
    " · in the repository's image",
  )
  expect(sessionImage({ image: null, image_note: 'podman build exited 1' })).toBe(
    " · in the harness image, not the repository's: podman build exited 1",
  )
  expect(sessionImage({ image: null, image_note: null })).toBe('')
  expect(sessionImage({})).toBe('')
})

test('the repetition hint stands only until the harness moves on', () => {
  const signal = ev('repetition', null, { count: 3, title: 'run just test' })
  expect(repetitionHint([ev('tool_call'), signal])).toBe(
    'same call 3× in a row · run just test · recorded, not paused',
  )
  // A turn ending, a pause or a resume all end the run it was describing.
  expect(repetitionHint([signal, ev('turn_end')])).toBeNull()
  expect(repetitionHint([signal, ev('session_paused')])).toBeNull()
  expect(repetitionHint([])).toBeNull()
})

test('a usage disagreement shows both numbers and what was charged', () => {
  expect(
    usageMismatchLine({
      gateway: { tokens: 10000, requests: 4 },
      harness: { tokens: 12 },
      charged_tokens: 10000,
    }),
  ).toBe('usage disagrees · gateway 10k · harness 12 · charged 10k')
  // A harness that said nothing is not a harness that said zero.
  expect(usageMismatchLine({ gateway: { tokens: 50 }, harness: { tokens: null }, charged_tokens: 50 })).toBe(
    'usage disagrees · gateway 50 · harness reported nothing · charged 50',
  )
})

test('an unmetered turn is named as unknown, never as zero', () => {
  const line = usageUnmeteredLine({ gateway: { tokens: 0, requests: 3 } })
  expect(line).toContain('3 model calls')
  expect(line).toContain('unknown rather than zero')
  expect(usageUnmeteredLine({ gateway: { requests: 1 } })).toContain('1 model call')
})

test('an orientation that lost context names how many pieces, not just that it was cut', () => {
  const payload = {
    chars: 22812,
    trimmed: true,
    missing: [
      { what: 'guide "Workspace" (`guide-workspace`)', partial: true, chars: 8000, fetch: 'call `doc_read` for `guide-workspace`' },
      { what: 'guide "Release" (`guide-release`)', partial: false, chars: 16000, fetch: 'call `doc_read` for `guide-release`' },
    ],
  }
  expect(orientationLine(payload)).toBe('orientation · 23k chars · 2 pieces not included in full')
  expect(orientationLine({ chars: 900 })).toBe('orientation · 900 chars')
  expect(orientationLine({ chars: 1, missing: [{ what: 'the diff', partial: true, chars: 40 }] })).toContain(
    '1 piece not included in full',
  )

  // Each omission names the document and how to fetch it: an operator cannot
  // act on "something was trimmed".
  const [cut, absent] = missingContext(payload)
  expect(missingLine(cut)).toBe(
    'guide "Workspace" (`guide-workspace`) cut short, 8k chars — call `doc_read` for `guide-workspace`',
  )
  expect(missingLine(absent)).toContain('not included, 16k chars')
  expect(missingLine({ what: '3 more directives and facts', partial: false, chars: 8000 })).toBe(
    '3 more directives and facts not included, 8k chars',
  )
})

test('an orientation event from an older node carries no omissions', () => {
  expect(missingContext({ chars: 100, trimmed: false })).toEqual([])
  expect(missingContext({ missing: 'not a list' })).toEqual([])
  expect(missingContext({ missing: [null, 7, { what: 'the plan', partial: true, chars: 5 }] })).toHaveLength(1)
  expect(orientationLine({ chars: 100, trimmed: true })).toBe('orientation · 100 chars · trimmed')
  expect(orientationLine({ chars: 100, trimmed: false })).toBe('orientation · 100 chars')
})

test('an orientation names the context revision it carried and what moved since the last attempt', () => {
  const payload = {
    chars: 900,
    context: {
      revision: 2,
      previous_revision: 1,
      changes: [{ kind: 'edited', slug: 'brief-a', role: 'brief', says: '`brief-a` edited since the previous attempt' }],
      omitted: ['guide-b'],
    },
  }
  expect(orientationLine(payload)).toBe('orientation · 900 chars · context revision 2 · 1 changed')
  expect(orientationContext(payload)?.omitted).toEqual(['guide-b'])
  expect(orientationLine({ chars: 900, context: { revision: 1, previous_revision: null, changes: [], omitted: [] } })).toBe(
    'orientation · 900 chars · context revision 1',
  )
  // An orientation from before contexts were selected, or for an item with none.
  expect(orientationContext({ chars: 1 })).toBeNull()
  expect(orientationContext({ context: null })).toBeNull()
})

// Every kind the node writes to a session's log, read from the Rust module
// that names them, so a kind added there without a line here fails this test
// rather than printing as a bare identifier.
function nodeEventKinds(): string[] {
  const rust = readFileSync(new URL('../../../node/src/session/state.rs', import.meta.url), 'utf8')
  const start = rust.indexOf('pub mod event_kind {')
  const end = rust.indexOf('\n}\n', start)
  expect(start).toBeGreaterThan(-1)
  return [...rust.slice(start, end).matchAll(/pub const [A-Z_]+: &str = "([a-z_]+)";/g)].map((m) => m[1])
}

// The kinds Log.svelte gives markup of its own.
function markedUpKinds(): Set<string> {
  const svelte = readFileSync(new URL('../components/Log.svelte', import.meta.url), 'utf8')
  return new Set([...svelte.matchAll(/e\.kind === '([a-z_]+)'/g)].map((m) => m[1]))
}

test('every event kind the node records has a line of its own', () => {
  const kinds = nodeEventKinds()
  // A sanity floor: a parse that found nothing would pass vacuously.
  expect(kinds.length).toBeGreaterThan(40)
  expect(kinds).toContain('provider_exhausted')
  const markup = markedUpKinds()
  // Tool calls fold into groups; the silent kinds render nothing on purpose.
  const handled = (kind: string) =>
    kind === 'tool_call' || SILENT.has(kind) || markup.has(kind) || eventLine(ev(kind)) !== null
  expect(kinds.filter((k) => !handled(k))).toEqual([])
})

test('each 0.29.0 kind reads as a sentence, not its name', () => {
  const lines: [string, Record<string, unknown>, string][] = [
    ['service', { name: 'postgres', container: 'c', state: 'ready' }, 'service postgres ready'],
    ['service', { name: 'postgres', state: 'failed', detail: 'did not answer on 5432 within 30s' }, 'service postgres failed · did not answer on 5432 within 30s'],
    ['published', { url: 'https://github.com/o/r/pull/418', review_id: 'rev-1' }, 'published · o/r/pull/418'],
    ['session_suspended', { idle_ms: 1_800_000 }, 'suspended · idle 30m after publishing'],
    ['session_paused', { source: 'operator', reason: 'operator paused the session' }, 'paused by the operator'],
    ['session_paused', { source: 'watchdog', reason: '3 failed tool calls in a row' }, 'paused by the node · 3 failed tool calls in a row'],
    ['session_paused', { source: 'exhausted', reason: 'anthropic is exhausted (…)' }, 'paused · its provider is exhausted'],
    ['session_resumed', { source: 'wake', reason: "the provider's limit has reset" }, "resumed by the node · the provider's limit has reset"],
    ['session_resumed', { source: 'operator', reason: 'operator resumed the session' }, 'resumed by the operator'],
    ['exhaustion_boundary', { boundary_seq: 42, outcome: 'waiting' }, 'the cut-off turn has settled · the work resumes from here'],
    ['exhaustion_wake', { outcome: 'resumed' }, "the provider's limit reset · resumed"],
    ['exhaustion_wake', { outcome: 'held', note: 'The node did not carry on: no model.' }, 'held for you · The node did not carry on: no model.'],
    ['approval_settled', { tool: 'pr_merge', state: 'succeeded', reason: 'x' }, 'approved call succeeded · pr_merge'],
    ['approval_settled', { tool: 'pr_merge', state: 'failed', reason: 'not mergeable' }, 'approved call failed · pr_merge · not mergeable'],
    ['candidate_verified', { candidate_id: 'c', head_sha: '8c4be91f03a2d6e7b5c1', reused: true }, 'checks passed · at 8c4be91f03a2 · evidence reused'],
    ['review_decision', { source: 'operator', review_id: 'r', decision: 'approved', waiting_ms: 720_000 }, 'review approved · by the operator · after 12m waiting'],
    ['review_decision', { decision: 'revise', waiting_ms: 5000 }, 'review sent back with notes · by the operator · after 5s waiting'],
    ['uncertain', { reason: 'the prompt never reported', refusing: 'prompt' }, 'outcome unknown · the prompt never reported · new prompts refused until it settles'],
    ['uncertain', { cleared: 'the turn ended' }, 'outcome settled · the turn ended'],
    ['pty_opened', { phase: 'spawn', command: 'bash', args: ['-l'], cwd: '/w' }, 'terminal opened · bash -l · in /w · what is typed in it is not recorded'],
    ['pty_closed', { phase: 'detached', duration_ms: 65_000 }, 'terminal detached · after 1m'],
    ['abandoned', { reason: 'item closed', summary: 'superseded by #12' }, 'abandoned · superseded by #12'],
  ]
  for (const [kind, payload, text] of lines) expect(eventLine(ev(kind, null, payload))?.text).toBe(text)
})

test('a published line links the forge, and a continued wake links the new session', () => {
  const published = eventLine(ev('published', null, { url: 'https://github.com/o/r/pull/418' }))!
  expect(published.href).toBe('https://github.com/o/r/pull/418')
  expect(published.tone).toBe('ok')
  const wake = eventLine(ev('exhaustion_wake', null, { outcome: 'continued', continued_by: '0b1c2d3e-4f50', model: 'openai/gpt-5' }))!
  expect(wake.text).toBe('continued in a new session · on openai/gpt-5')
  expect(wake.href).toBe('/sessions/0b1c2d3e-4f50')
  expect(wake.link).toBe('0b1c2d3e')
})

test('the exhaustion line says what refused and what happens next', () => {
  const held = eventLine(
    ev('provider_exhausted', null, { policy: 'pause', outcome: 'held', provider: 'anthropic', reason: 'usage limit reached', note: 'no fallback' }),
  )!
  expect(held.text).toBe('anthropic is exhausted · usage limit reached · waiting for you · no fallback')
  const falling = eventLine(ev('provider_exhausted', null, { outcome: 'falling_back', provider: 'anthropic', fallback: 'openai/gpt-5' }))!
  expect(falling.text).toBe('anthropic is exhausted · carrying on with openai/gpt-5 from where it stopped')
})

test('a kind nobody taught the log still reads as words with its short fields', () => {
  expect(eventLine(ev('brand_new_kind'))).toBeNull()
  const line = fallbackLine(ev('brand_new_kind', null, { name: 'postgres', nested: { a: 1 }, ready: true, count: 3, extra: 'x' }))
  expect(line.text).toBe('brand new kind · name postgres · ready true · count 3')
  expect(line.tone).toBe('sys')
})

test("a workspace's preparation reads as what it did and what it cost the start", () => {
  const commands = ['cargo fetch --locked', 'cd spa && bun install --frozen-lockfile']
  const lines: [Record<string, unknown>, string, string][] = [
    [{ outcome: 'prepared', commands, ms: 11_200 }, 'workspace prepared · cargo fetch --locked · cd spa && bun install --frozen-lockfile · 11s', 'mark'],
    [{ outcome: 'failed', commands, ms: 4_000, detail: 'preparation `cargo fetch --locked` failed (exit 101)' }, 'workspace not prepared · preparation failed · the session started without it · 4s', 'crit'],
    [{ outcome: 'timed_out', commands, ms: 2_400_000, timeout_secs: 2400 }, 'workspace not prepared · timed out after 40m · the session started without it', 'crit'],
    [{ outcome: 'cancelled', commands, ms: 900, reason: 'the session was stopped' }, 'workspace preparation stopped · the session was stopped', 'mark'],
    [{ outcome: 'not_run', commands, ms: 0, reason: "the session runs in the harness's own image, not its repository's" }, "workspace not prepared · the session runs in the harness's own image, not its repository's", 'sys'],
  ]
  for (const [payload, text, tone] of lines) {
    expect(eventLine(ev('workspace_prepared', null, payload))).toEqual({ text, tone: tone as 'mark' })
  }
})
