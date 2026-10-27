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
  queuedDeliveries,
  queuedStatus,
  groupRunning,
  groupSummary,
  policyLine,
  providerErrorLine,
  repetitionHint,
  repetitionLine,
  sessionImage,
  usageMismatchLine,
  usageUnmeteredLine,
  worktreeLine,
  worktreeOrigin,
} from './log'
import type { Event } from './types'
import { formatTokens } from './format'

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

test('the worktree line says the workspace is prepared, and only what needs the operator', () => {
  // A workspace imported into a runtime volume: where it came from is folded.
  const imported = { branch: 'feat/tracon-991d226c9df2', source: 'imported', volume: 'tracon-workspace-991d226c9df2', workspace_id: '991d226c9df2' }
  expect(worktreeLine(imported)).toBe('workspace prepared')
  expect(worktreeOrigin(imported)).toBe('imported · on feat/tracon-991d226c9df2 · volume tracon-workspace-991d226c9df2')
  expect(worktreeLine({ branch: 'feat/x', source: 'resumed', volume: 'tracon-workspace-abc' })).toBe('workspace resumed')
  // A source this does not know still reads as words, not as a field name.
  expect(worktreeOrigin({ branch: 'feat/x', source: 'host_worktree' })).toBe('host worktree · on feat/x')
  // A host worktree, which names its path and the base it started from.
  expect(
    worktreeOrigin({ path: '/var/lib/tracon/worktrees/orbit', branch: 'feat/x', base: 'origin/main', main_checkout_dirty: false }),
  ).toBe('host worktree · /var/lib/tracon/worktrees/orbit · on feat/x · from origin/main')
  expect(worktreeLine({ path: '/w/orbit', branch: 'feat/x', base: 'origin/main', main_checkout_dirty: true })).toBe(
    'workspace prepared · main checkout is dirty and was left alone',
  )
  expect(worktreeLine({})).toBe('workspace prepared')
  expect(worktreeOrigin({})).toBe('')
  // A managed clone that could not be fetched says its base is stale.
  expect(
    worktreeLine({
      branch: 'feat/x',
      source: 'imported',
      synced: { source_ref: 'origin/main', stale: 'stale: last fetched at 2026-10-07 12:00 UTC; the fetch failed: x' },
    }),
  ).toBe('workspace prepared · stale: last fetched at 2026-10-07 12:00 UTC; the fetch failed: x')
  expect(worktreeLine({ source: 'imported', synced: { stale: null } })).toBe('workspace prepared')
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
    [
      'session_paused',
      { source: 'budget', reason: 'reached its token budget: 1500 of 1000 tokens; raise the budget to carry on' },
      'paused · reached its token budget: 1500 of 1000 tokens; raise the budget to carry on',
    ],
    ['budget_changed', { from: 1000, to: 6000, tokens_used: 1500 }, 'budget raised · 1k → 6k tokens'],
    ['budget_changed', { from: 2_000_000, to: 0, tokens_used: 1500 }, 'budget removed · 2M → no budget'],
    ['budget_changed', { from: 0, to: 4_000_000, tokens_used: 1500 }, 'budget set · no budget → 4M tokens'],
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
    ['review_taken_over', { review_id: 'r', state: 'revising', from_session: '0b1c2d3e-4f50', to_session: 's2' }, 'review sent back for changes taken over · from 0b1c2d3e'],
    ['review_taken_over', { review_id: 'r', state: 'new' }, 'open review taken over'],
    [
      'review_decision',
      { source: 'operator', review_id: 'r', decision: 'approved', reconciled: 'https://forge.test/pull/7' },
      'review recorded as published · at https://forge.test/pull/7 · by the operator',
    ],
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

test("a check run's preparation reads as itself, and a failed one names its command and the checks it kept from running", () => {
  expect(eventLine(ev('check_prepared', null, { ok: true, ms: 11_200 }))).toEqual({
    text: 'dependencies prepared · once for this run · 11s',
    tone: 'mark',
  })
  const failed = {
    ok: false,
    ms: 4_000,
    reason: 'preparation failed: `composer install` (exit 100)',
    command: 'composer install',
    exit: 100,
    not_run: ['./vendor/bin/pint --test', './vendor/bin/pest'],
  }
  expect(eventLine(ev('check_prepared', null, failed))).toEqual({
    text: 'preparation failed: `composer install` (exit 100) · no check was run: ./vendor/bin/pint --test, ./vendor/bin/pest · 4s',
    tone: 'crit',
  })
  // Recorded before a failure carried its reason: still never a check's.
  expect(eventLine(ev('check_prepared', null, { ok: false, ms: 0 }))).toEqual({
    text: 'preparation failed · no check was run · 0s',
    tone: 'crit',
  })
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

test('a queued message says where it went, or that it is still waiting', () => {
  const at = (kind: string, payload: Record<string, unknown>, at_ms: number) => ({ ...ev(kind, null, payload), at_ms })
  const events = [
    at('prompt_queued', { message_id: 'm1', text: 'use staging' }, 1),
    at('prompt_queued', { message_id: 'm2', text: 'rerun it' }, 2),
    at('prompt_queued', { message_id: 'm3', text: 'and say so' }, 3),
    at('prompt_delivered', { message_ids: ['m1'], tool: 'review_status' }, 4),
    at('user_prompt', { text: 'rerun it', queued: ['m2'] }, 5),
  ]
  const delivered = queuedDeliveries(events)
  expect(delivered.get('m1')).toEqual({ via: 'tool', tool: 'review_status', at_ms: 4 })
  expect(delivered.get('m2')).toEqual({ via: 'prompt', at_ms: 5 })
  expect(delivered.has('m3')).toBe(false)
  expect(queuedStatus(delivered.get('m1'), false, '14:02')).toBe('delivered · in the review_status result · 14:02')
  expect(queuedStatus(delivered.get('m2'), false)).toBe('delivered · as the next prompt')
  expect(queuedStatus(undefined, false)).toBe(
    'queued · the agent gets it in its next tracon tool result, or when this turn ends',
  )
  expect(queuedStatus(undefined, true)).toBe('queued · never delivered: the session ended first')
  expect(eventLine(events[3])?.text).toBe('your queued message reached the agent · in the review_status result')
  expect(eventLine(ev('prompt_delivered', null, { message_ids: ['a', 'b'], tool: 'run_wait' }))?.text).toBe(
    'your 2 queued messages reached the agent · in the run_wait result',
  )
})

test('a harness restart says what became of the session', () => {
  const restored = eventLine(ev('harness_restarted', null, { outcome: 'restored', exit_code: 137, interrupted_turn: true }))
  expect(restored?.text).toBe('the harness stopped (exit 137) · restarted on this session; the cut turn carries on')
  const refused = eventLine(ev('harness_restarted', null, { outcome: 'refused', reason: 'the harness died again after 3 restarts; the session ends' }))
  expect(refused?.tone).toBe('crit')
  expect(refused?.text).toContain('after 3 restarts')
})

test('a compaction says how much smaller the context got and what went', () => {
  const summarised = eventLine(
    ev('context_compacted', null, { trigger: 'threshold', tokens_before: 49_800, tokens_after: 6_200, summarised_messages: 34, pruned_results: 0, summary: 'did things' }),
  )
  expect(summarised?.tone).toBe('sys')
  expect(summarised?.text).toBe(`context compacted · ~${formatTokens(49_800)} → ~${formatTokens(6_200)} tokens · 34 earlier messages summarised`)
  const pruned = eventLine(ev('context_compacted', null, { trigger: 'overflow', tokens_before: 70_000, tokens_after: 40_000, summarised_messages: 0, pruned_results: 1 }))
  expect(pruned?.text).toContain('after the model refused it as too long')
  expect(pruned?.text).toContain('1 old tool output cut')
})

test('a compaction the operator asked for says so', () => {
  const manual = eventLine(ev('context_compacted', null, { trigger: 'manual', tokens_before: 48_000, tokens_after: 3_100, summary: 'the ledger is half done' }))
  expect(manual?.text).toBe(`context compacted on request · ~${formatTokens(48_000)} → ~${formatTokens(3_100)} tokens`)
})

test('asking an ended session again says it is the same conversation, or why it is not', () => {
  const resumed = eventLine(ev('conversation_resumed', null, { outcome: 'resumed', from_state: 'closed', end_reason: 'killed_user' }))
  expect(resumed?.text).toBe('asked again after it ended · in its own conversation')
  const compacting = eventLine(ev('conversation_resumed', null, { outcome: 'resumed', from_state: 'suspended', compact: true }))
  expect(compacting?.text).toBe('asked again after it was suspended · compacting its conversation first')
  const gone = eventLine(
    ev('conversation_resumed', null, { outcome: 'failed', gone: true, from_state: 'closed', reason: 'the claude harness no longer has a record of conversation c1' }),
  )
  expect(gone?.tone).toBe('crit')
  expect(gone?.text).toBe('its conversation cannot be restored · the claude harness no longer has a record of conversation c1 · left as it ended')
})

test("a subagent's calls and asks sit under the spawn, not in the parent's run", () => {
  const log = groupLog([
    ev('tool_call', 'c1', { title: 'subagent: look', kind: 'think' }),
    ev('policy_allowed', null, { kind: 'think', tool_call_id: 'c1' }),
    ev('tool_call', 'c1/a', { title: '[look] read src/lib.rs', parent: 'c1' }),
    ev('policy_allowed', null, { kind: 'read', tool_call_id: 'c1/a', parent: 'c1' }),
    ev('tool_call', 'c1/b', { title: '[look] shell cargo test', parent: 'c1' }),
    ev('permission_request', 'p1', { title: '[look] shell cargo test', tool_call_id: 'c1/b', parent: 'c1' }),
    ev('tool_result', 'c1/a', { status: 'completed', output: 'fn main() {}' }),
    ev('tool_call', 'c2', { title: 'read README.md', kind: 'read' }),
    ev('tool_result', 'c2', { status: 'completed' }),
  ])
  // One run: the child's ask did not break the parent's calls apart.
  expect(log.map((e) => e.kind)).toEqual(['tools'])
  const [spawn, read] = log[0].tools!
  expect(spawn.call.ref_id).toBe('c1')
  expect(spawn.kind).toBe('think')
  expect(read.call.ref_id).toBe('c2')
  const children = spawn.children!
  expect(children.map((e) => e.kind)).toEqual(['tools', 'leaf'])
  expect(children[0].tools!.map((t) => [t.call.ref_id, t.result?.payload.status, t.kind])).toEqual([
    ['c1/a', 'completed', 'read'],
    ['c1/b', undefined, undefined],
  ])
  expect(children[1].event!.kind).toBe('permission_request')
})
