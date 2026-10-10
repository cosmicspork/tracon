// The sessions list and a session across its lifecycle: starting, working,
// waiting on a permission / the operator / a check, idle, paused, exhausted,
// suspended after publishing, ended every way it can end, and the OpenCode
// shell around a session. Each state overrides the session list (the session
// screen reads its row from there), the detail, the events and whatever the
// panels on the page fetch.

import { flaky, streamEvent } from '../flaky.mjs'

const SELF = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
const PEER = '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1'

// Durations that end in `_ms` (asleep_ms, idle_ms, duration_ms) must not be
// turned into timestamps, which the runner's stamp would do. Bodies holding
// them are stamped here, at request time, and sent as JSON text (api.ts parses
// any body as JSON whatever its content type).
const DURATIONS = new Set(['asleep_ms', 'idle_ms', 'duration_ms', 'cookie_ttl_ms'])
const stampExact = (v) => {
  if (Array.isArray(v)) return v.map(stampExact)
  if (v !== null && typeof v === 'object') {
    return Object.fromEntries(
      Object.entries(v).map(([k, val]) => [
        k,
        k.endsWith('_ms') && !DURATIONS.has(k) && typeof val === 'number' && Math.abs(val) < 1e12
          ? Date.now() + val
          : stampExact(val),
      ]),
    )
  }
  return v
}
const exact = (value) => () => JSON.stringify(stampExact(value))
const never = () => new Promise(() => {})
const fail = (status, message) => ({ status, body: { error: { code: status, message } } })

function session(id, o = {}) {
  return {
    id,
    node_id: SELF,
    channel: 'personal',
    work_item_id: 'wi-2',
    repo_path: '/home/op/src/orbit',
    worktree_path: `/var/lib/tracon/worktrees/orbit-${id.slice(0, 6)}`,
    branch: 'feat/rate-limits',
    harness_id: 'claude',
    harness_version: '2.5.0',
    harness_agent: 'claude-code',
    harness_found: '2.5.0',
    harness_protocol: 'acp/1',
    harness_session_id: null,
    model: 'sonnet',
    phase: 'execute',
    policy_version: 12,
    review_id: null,
    budget_tokens: 2000000,
    tokens_used: 412000,
    cost_usd: 2.87,
    context_used: 84000,
    context_size: 200000,
    state: 'running',
    end_reason: null,
    last_error: null,
    turn_active: 0,
    draft: null,
    created_ms: -2700000,
    updated_ms: -140000,
    archived_ms: null,
    legacy_ms: null,
    parent_session: null,
    continued_from: null,
    manifest_digest: 'b71e04c9d2a85f3e6c0b9d47a1e28f53c6d9b0a4e7f1c2d8a5b3e6f9c0d1a2b4',
    ...o,
  }
}

const usageReconciled = {
  state: 'reconciled',
  gateway_tokens: 412000,
  harness_tokens: 409800,
  charged_tokens: 412000,
  cache_read_tokens: 1840000,
  unmetered_turns: 0,
  mismatched_turns: 0,
  turns: [],
}
const ceilingUnder = { usage_today: 412000, ceiling: 4000000, state: 'under', unmetered_turns: 0 }
const toolchainReady = {
  revision: 'tc-7',
  image_revision: 'tc-7',
  tools: [
    { id: 'rust-analyzer', kind: 'lsp', version: '2026-09-29', path: '/opt/tools/rust-analyzer', state: 'configured' },
    { id: 'rustfmt', kind: 'formatter', version: '1.90.0', path: '/opt/tools/rustfmt', state: 'configured' },
    { id: 'typescript-language-server', kind: 'lsp', version: '4.4.0', path: '/opt/tools/tsls', state: 'configured' },
  ],
}

function detail(s, o = {}) {
  return {
    session: s,
    waiting: [],
    questions: [],
    usage: usageReconciled,
    ceiling: ceilingUnder,
    toolchain: s.harness_id === 'opencode' ? toolchainReady : null,
    exhaustion: null,
    shown_work: [],
    publications: [],
    ...o,
  }
}

// ---- events ---------------------------------------------------------------

let seq = 0
function ev(sid, kind, payload = {}, at_ms = -2600000, ref_id = null) {
  seq += 1
  return { seq, node_id: SELF.slice(0, 6), session_id: sid, kind, ref_id, payload, at_ms, mono_ms: seq * 1000 }
}

function opening(sid, o = {}) {
  return [
    ev(sid, 'session_started', { model: o.model ?? 'sonnet', phase: o.phase ?? 'execute', policy_version: 12, image: 'tracon-repo/orbit:9c1e4a' }, -2700000),
    ev(
      sid,
      'orientation',
      {
        chars: 14820,
        trimmed: false,
        text: 'You are executing work item wi-2 on channel personal. The plan is plan-rate-limits. Directives: worktree only, review before publish, no merge.',
        context: { revision: 3, previous_revision: null, changes: [], omitted: [] },
      },
      -2700000,
    ),
    ev(sid, 'worktree', { path: `/var/lib/tracon/worktrees/orbit-${sid.slice(0, 6)}`, branch: o.branch ?? 'feat/rate-limits', base: 'origin/main', main_checkout_dirty: false }, -2699000),
    ev(sid, 'user_prompt', { text: o.prompt ?? 'Add per-channel rate limits to the gateway. The plan is in plan-rate-limits; the review binding stays untouched.' }, -2640000),
  ]
}

// Claude Code reads without asking; since #383 each such call is recorded as
// the node's decision anyway, decided_by: harness.
function claudeRead(sid, n, path, at) {
  const id = `toolu_${sid.slice(0, 4)}${n}`
  return [
    ev(sid, 'tool_call', { title: `Read ${path}`, kind: 'read' }, at, id),
    ev(
      sid,
      'policy_allowed',
      { title: `Read ${path}`, action: 'read', kind: 'read', resource: path, command: null, rule: 'workspace-read', reason: 'Reading inside the workspace.', tool_call_id: id, decided_by: 'harness', policy: 'allow' },
      at + 200,
    ),
    ev(sid, 'tool_result', { status: 'completed' }, at + 400, id),
  ]
}

// A Claude Code call as the node records it: the tool's name as the title,
// `other` as the kind (the policy record says what it did), the input as the
// model wrote it, and the output serialized as JSON. `output: undefined`
// leaves the call running.
function claudeCall(sid, id, name, kind, input, at, ms, o = {}) {
  const events = [
    ev(sid, 'tool_call', { title: name, kind: 'other', status: 'in_progress', raw_input: input, locations: [] }, at, id),
    ev(sid, 'policy_allowed', { title: name, action: kind, kind, resource: input.file_path ?? input.path ?? null, command: input.command ?? null, rule: kind === 'execute' ? 'cargo-local' : 'workspace-read', reason: '', tool_call_id: id, decided_by: 'harness', policy: 'allow' }, at + 50),
  ]
  if (o.output !== undefined) {
    const output = o.cut ? JSON.stringify(o.output).slice(0, o.cut) : JSON.stringify(o.output)
    events.push(ev(sid, 'tool_result', { status: o.failed ? 'failed' : 'completed', output, truncated: !!o.cut }, at + ms, id))
  }
  return events
}

function transcriptEvents(sid) {
  const wt = `/var/lib/tracon/worktrees/orbit-${sid.slice(0, 6)}`
  const src = (f) => `${wt}/node/src/${f}`
  const rust = (n) => Array.from({ length: n }, (_, i) => `${String(i + 1).padStart(4)}\t// line ${i + 1}`).join('\n')
  return [
    ...opening(sid, { prompt: 'Add per-channel rate limits to the gateway, reusing the ceiling window.' }),
    ...claudeCall(sid, 'toolu_t1', 'Read', 'read', { file_path: src('gateway/model.rs') }, -840000, 300, { output: rust(40) }),
    ...claudeCall(sid, 'toolu_t2', 'Read', 'read', { file_path: src('metrics.rs') }, -838000, 200, { output: rust(30) }),
    ...claudeCall(sid, 'toolu_t3', 'Read', 'read', { file_path: src('gateway/mod.rs'), offset: 200, limit: 80 }, -836000, 250, { output: rust(400), cut: 900 }),
    ...claudeCall(sid, 'toolu_t4', 'Grep', 'search', { pattern: 'fn ceiling', path: `${wt}/node/src` }, -834000, 400, { output: 'node/src/metrics.rs:118:pub fn ceiling(&self, channel: &str) -> Option<u64> {' }),
    ...claudeCall(sid, 'toolu_t5', 'Edit', 'edit', { file_path: src('gateway/model.rs'), old_string: 'let used = 0;', new_string: 'let used = window.used(channel);' }, -800000, 600, { output: 'The file node/src/gateway/model.rs has been updated.' }),
    ...claudeCall(sid, 'toolu_t6', 'Bash', 'execute', { command: 'cargo build -p tracon-node', description: 'Build the node' }, -790000, 48200, {
      failed: true,
      output: '   Compiling tracon-node v0.30.0 (/work/node)\nerror[E0425]: cannot find value `window` in this scope\n   --> node/src/gateway/model.rs:212:20\n    |\n212 |         let used = window.used(channel);\n    |                    ^^^^^^ not found in this scope\n\nerror: could not compile `tracon-node` (lib) due to 1 previous error',
    }),
    ev(sid, 'message', { text: 'The window lives on the metrics side; taking it from the gateway state instead and building again.' }, -130000),
    ...claudeCall(sid, 'toolu_t7', 'Edit', 'edit', { file_path: src('gateway/model.rs'), old_string: 'window.used', new_string: 'self.metrics.window().used' }, -120000, 500, { output: 'The file node/src/gateway/model.rs has been updated.' }),
    ...claudeCall(sid, 'toolu_t8', 'Read', 'read', { file_path: src('metrics.rs'), offset: 100, limit: 40 }, -118000, 200, { output: rust(40) }),
    ...claudeCall(sid, 'toolu_t9', 'Bash', 'execute', { command: 'cargo test -p tracon-node gateway\n  && cargo clippy -p tracon-node', description: 'Test the gateway' }, -95000, 0),
  ]
}

function workTurn(sid, at = -2620000) {
  return [
    ev(sid, 'thought', { text: 'The ceiling check in metrics.rs already counts tokens per channel per day. A limiter can reuse that window instead of keeping its own.' }, at),
    ...claudeRead(sid, 1, 'node/src/gateway/model.rs', at + 2000),
    ...claudeRead(sid, 2, 'node/src/metrics.rs', at + 4000),
    ev(sid, 'tool_call', { title: 'Grep "ceiling(" in node/src', kind: 'search' }, at + 6000, `toolu_${sid.slice(0, 4)}g`),
    ev(sid, 'policy_allowed', { title: 'Grep "ceiling(" in node/src', action: 'search', kind: 'search', resource: 'node/src', command: null, rule: 'workspace-read', reason: 'Reading inside the workspace.', tool_call_id: `toolu_${sid.slice(0, 4)}g`, decided_by: 'harness', policy: 'allow' }, at + 6100),
    ev(sid, 'tool_result', { status: 'completed' }, at + 6400, `toolu_${sid.slice(0, 4)}g`),
    ev(sid, 'message', { text: "Reading the gateway's request path first. The per-channel window in metrics.rs already has the counters this needs, so the limiter reuses it — no new table. Two files will change: the gateway's request path and the window's read side." }, at + 20000),
    ev(sid, 'tool_call', { title: 'Edit node/src/gateway/model.rs', kind: 'edit' }, at + 60000, `toolu_${sid.slice(0, 4)}e`),
    ev(sid, 'tool_result', { status: 'completed' }, at + 61000, `toolu_${sid.slice(0, 4)}e`),
    ev(sid, 'tool_call', { title: 'Edit node/src/metrics.rs', kind: 'edit' }, at + 62000, `toolu_${sid.slice(0, 4)}f`),
    ev(sid, 'tool_result', { status: 'completed' }, at + 63000, `toolu_${sid.slice(0, 4)}f`),
    ev(sid, 'turn_end', { usage: { total_tokens: 84213 } }, at + 120000),
  ]
}

// ---- panels a page fetches --------------------------------------------------

function authority(s, o = {}) {
  return {
    session_id: s.id,
    channel: s.channel,
    node: { id: s.node_id, name: s.node_id === SELF ? 'laptop' : 'work-pod', is_self: s.node_id === SELF, reachable: true, isolation: 'podman (rootless, gateway egress)', failed_check: null, failed_detail: null },
    harness: { id: s.harness_id, expected: s.harness_version, found: s.harness_found, agent: s.harness_agent, protocol: s.harness_protocol },
    image: { runtime: 'podman', execution: 'tracon-repo/orbit:9c1e4a', manifest_digest: s.manifest_digest },
    access: {
      credentials: ['anthropic (model, via gateway)', 'github · cosmic-example (review publication only)'],
      egress: ['crates.io', 'static.crates.io', 'index.crates.io', 'registry.npmjs.org'],
      workspace: s.worktree_path,
      repo: s.repo_path,
      branch: s.branch,
      external_broker: false,
    },
    limits: { budget_tokens: s.budget_tokens, tokens_used: s.tokens_used, permission_timeout_secs: 900, ceiling: ceilingUnder },
    policy: { version: 12, trusted: true, rules: 41 },
    actions: [
      { name: 'read', surface: 'tool', verdict: 'allow', rule_id: 'workspace-read', reason: 'Reading inside the workspace.', scoped: [] },
      { name: 'edit', surface: 'tool', verdict: 'allow', rule_id: 'workspace-edit', reason: 'Editing inside the worktree.', scoped: [] },
      {
        name: 'execute',
        surface: 'tool',
        verdict: 'ask',
        rule_id: null,
        reason: null,
        scoped: [{ rule_id: 'cargo-local', reason: 'Local builds and tests.', args: { command: ['cargo build*', 'cargo test*', 'cargo clippy*', 'just check', 'just test'] } }],
      },
      { name: 'fetch', surface: 'tool', verdict: 'ask', rule_id: null, reason: null, scoped: [] },
      { name: 'publish', surface: 'authority', verdict: 'ask', rule_id: 'publish-review', reason: 'Publication goes through review.', scoped: [], grants: [] },
      {
        name: 'merge',
        surface: 'authority',
        verdict: 'ask',
        rule_id: null,
        reason: null,
        scoped: [],
        grants: [{ id: 'g-1', action: 'merge', verdict: 'allow', target: 'cosmic-example/orbit#412', channel: 'personal', session_id: null, revision: '8c4be91', expires_ms: 3600000, revoked_ms: null, reason: 'Release PR, approved in chat', created_ms: -600000 }],
      },
      { name: 'deploy', surface: 'authority', verdict: 'deny', rule_id: 'no-deploy', reason: 'Deploys happen on merge, never from a session.', scoped: [] },
      { name: 'git push', surface: 'capability', verdict: 'deny', rule_id: 'no-direct-push', reason: 'The node publishes approved reviews; a session never pushes.', scoped: [] },
    ],
    unattended_commands: [
      { rule_id: 'operator-checks', reason: "The operator's required checks, exactly as configured.", commands: ['just check', 'just test'], any: false },
    ],
    grants: [],
    ...o,
  }
}

function outcome(s, o = {}) {
  return {
    session_id: s.id,
    channel: s.channel,
    state: s.state,
    end_reason: s.end_reason,
    head_sha: null,
    changed: { reviews: [], files: [], added: 0, removed: 0, workspace_changes: 0 },
    verified: [],
    claims: [],
    needs_decision: [],
    uncertain: [],
    cost: { tokens_used: s.tokens_used, budget_tokens: s.budget_tokens, cost_usd: s.cost_usd, gateway_tokens: s.tokens_used, charged_tokens: s.tokens_used, unmetered_turns: 0, mismatched_turns: 0 },
    ...o,
  }
}

const outcomeDone = (s) =>
  outcome(s, {
    head_sha: '8c4be91f03a2d6e7b5c1f0a9e3d4b2c7a6f5e1d0',
    changed: {
      reviews: [{ id: 'rev-7', kind: 'pr', title: 'Per-channel rate limits in the gateway', state: 'published', head_sha: '8c4be91f03a2d6e7b5c1f0a9e3d4b2c7a6f5e1d0', added: 184, removed: 32, files: 4 }],
      files: ['node/src/gateway/model.rs', 'node/src/metrics.rs', 'node/tests/gateway.rs', 'docs/ARCHITECTURE.md'],
      added: 184,
      removed: 32,
      workspace_changes: 6,
    },
    verified: [
      { check_id: 'chk-1', command: 'just check', outcome: 'passed', source_outcome: null, head_sha: '8c4be91f03a2d6e7b5c1f0a9e3d4b2c7a6f5e1d0', passed: true, failed: false, current: true, finished_ms: -1700000 },
      { check_id: 'chk-2', command: 'just test', outcome: 'reused', source_outcome: 'passed', head_sha: '8c4be91f03a2d6e7b5c1f0a9e3d4b2c7a6f5e1d0', passed: true, failed: false, current: true, finished_ms: -1690000 },
      { check_id: 'chk-0', command: 'just test', outcome: 'failed', source_outcome: null, head_sha: '1d0e9a7c4b2f5e8d3a6c9b0f1e4d7a2c5b8e3f60', passed: false, failed: true, current: false, finished_ms: -2100000 },
    ],
    claims: [
      { source: 'review', id: 'rev-7', title: 'Per-channel rate limits in the gateway', text: 'Refusals read like the ceiling; tests cover the window boundary.', head_sha: '8c4be91f03a2d6e7b5c1f0a9e3d4b2c7a6f5e1d0', backed_by: ['chk-1', 'chk-2'], backed: true },
      { source: 'report', id: 'rep-2', title: 'Why the limiter reuses the ceiling window', text: 'No new table; the window already counts per channel.', head_sha: null, backed_by: [], backed: false },
    ],
  })

function continuation(s, o = {}) {
  return {
    kind: 'item',
    id: s.work_item_id ?? s.id,
    channel: s.channel,
    intent: { title: 'Per-channel rate limits in the gateway', body: '', source: 'item' },
    attempts: [
      { id: s.id, phase: s.phase, model: s.model, harness: s.harness_id, state: s.state, end_reason: s.end_reason, last_error: s.last_error, tokens_used: s.tokens_used, created_ms: s.created_ms, continued_from: s.continued_from ?? null, parent_session: null, archived: false },
    ],
    blockers: [],
    next: { kind: 'continue', text: 'Carry on from the last attempt’s workspace.', session_id: s.id },
    workspace: { id: `ws-${s.id}`, branch: s.branch, session_id: s.id },
    decisions: { plan: 'plan-rate-limits', brief: null, answered: [] },
    evidence: { reviews: [], shown: [] },
    actions: { continue_from: s.id, abandon: true },
    ...o,
  }
}

const NO_QUEUE = { waiting: [], reviews: [], promotions: [], running: [], ended: [] }

/** The api for one session screen. */
function page(s, o = {}) {
  const list = o.list ?? [s]
  const api = {
    'GET /api/sessions': list,
    [`GET /api/sessions/${s.id}`]: detail(s, o.detail),
    [`GET /api/sessions/${s.id}/events`]: o.events ?? [],
    [`GET /api/sessions/${s.id}/draft`]: { text: o.draft ?? '', updated_ms: o.draft ? -60000 : null },
    [`PUT /api/sessions/${s.id}/draft`]: { status: 204, body: null },
    [`GET /api/sessions/${s.id}/authority`]: o.authority ?? authority(s),
    [`GET /api/sessions/${s.id}/outcome`]: o.outcome ?? outcome(s),
    [`GET /api/sessions/${s.id}/continuation`]: o.continuation ?? continuation(s),
    'GET /api/queue': o.queue ?? NO_QUEUE,
    ...o.api,
  }
  return api
}

const perm = (sid, o = {}) => ({
  id: 'perm-41',
  session_id: sid,
  node_id: SELF,
  title: 'Bash: cargo test -p tracon-node --test gateway',
  kind: 'execute',
  raw_input: JSON.stringify({ command: 'cargo test -p tracon-node --test gateway', cwd: '/work', description: 'Run the gateway tests' }),
  options: JSON.stringify([
    { option_id: 'allow_once', name: 'Allow', kind: 'allow_once' },
    { option_id: 'allow_session', name: 'Allow cargo test for this session', kind: 'allow_session' },
    { option_id: 'reject_once', name: 'Deny', kind: 'reject_once' },
  ]),
  state: 'new',
  created_ms: -95000,
  expires_ms: 805000,
  intent: { channel: 'personal', phase: 'execute', branch: 'feat/rate-limits', work_item_id: 'wi-2', work_item_title: 'Per-channel rate limits in the gateway', session_state: 'waiting_on_you' },
  ...o,
})

// ---- the sessions list -----------------------------------------------------

const ROWS = [
  session('a1f0c3e2-7b4d-4e19-9c58-2d6b0f8e1a73', { state: 'starting', harness_found: null, harness_agent: null, harness_protocol: null, tokens_used: 0, cost_usd: 0, branch: 'fix/egress-retry', created_ms: -40000, updated_ms: -8000 }),
  session('b2e1d4f3-8c5e-4f2a-ad69-3e7c1a9f2b84', { state: 'running', turn_active: 1, harness_id: 'opencode', harness_version: '1.3.4', harness_agent: 'opencode', harness_found: '1.3.4', model: 'anthropic/claude-sonnet-4-5', branch: 'feat/service-sidecars', created_ms: -1800000, updated_ms: -4000 }),
  session('c3f2e5a4-9d6f-4a3b-be7a-4f8d2b0a3c95', { state: 'waiting_on_you', updated_ms: -95000 }),
  session('d4a3f6b5-ae7a-4b4c-8f8b-5a9e3c1b4da6', { state: 'waiting_on_check', branch: 'feat/outcome-record', created_ms: -5400000, updated_ms: -61000 }),
  session('e5b4a7c6-bf8b-4c5d-9a9c-6b0f4d2c5eb7', { state: 'paused', branch: 'chore/bump-axum', model: 'opus', tokens_used: 1310000, created_ms: -9000000, updated_ms: -3000000 }),
  session('f6c5b8d7-ca9c-4d6e-8bad-7c1a5e3d6fc8', { state: 'suspended', branch: 'fix/review-draft-race', created_ms: -14400000, updated_ms: -7200000 }),
  session('a7d6c9e8-dbad-4e7f-9cbe-8d2b6f4e7ad9', { node_id: PEER, channel: 'work', state: 'running', branch: 'feat/queue-metrics', repo_path: '/srv/repos/platform/orbit', created_ms: -3600000, updated_ms: -30000 }),
  session('17e8d0f9-ecbe-4f80-adcf-9e3c7a5f8be0', { state: 'closed', end_reason: 'node_restart', branch: 'feat/awake-inhibitor', created_ms: -20000000, updated_ms: -17000000 }),
  session('28f9e1a0-fdcf-4091-bed0-af4d8b6a9cf1', { state: 'closed', end_reason: 'provider_exhausted', branch: 'feat/exhaustion-policy', created_ms: -26000000, updated_ms: -24000000 }),
  session('39a0f2b1-0ed0-41a2-8fe1-b05e9c7b0d02', { state: 'closed', end_reason: 'continued', branch: 'feat/awake-inhibitor', created_ms: -40000000, updated_ms: -36000000 }),
  session('4ab1a3c2-1fe1-42b3-90f2-c16fad8c1e13', { state: 'closed', end_reason: 'item_close', branch: 'feat/session-cookies', created_ms: -93600000, updated_ms: -90000000 }),
  session('5bc2b4d3-20f2-43c4-a103-d27a0e9d2f24', { state: 'closed', end_reason: 'phase_done', phase: 'plan', branch: 'plan/repo-entry', created_ms: -100000000, updated_ms: -99000000 }),
  session('6cd3c5e4-3103-44d5-b214-e38b1fae3035', { state: 'closed', end_reason: 'phase_done', phase: 'review', model: 'opus', branch: 'feat/queue-metrics', review_id: 'rev-1', created_ms: -110000000, updated_ms: -109000000 }),
  session('7de4d6f5-4214-45e6-8325-f49c2a0f4146', { state: 'closed', end_reason: 'killed_user', branch: 'spike/pty-capture', created_ms: -120000000, updated_ms: -118000000 }),
  session('8ef5e7a6-5325-46f7-9436-a5ad3b1a5257', { state: 'killed_budget', tokens_used: 2004112, branch: 'refactor/store-split', created_ms: -130000000, updated_ms: -125000000 }),
  session('9fa6f8b7-6436-4708-a547-b6be4c2b6368', { state: 'failed', end_reason: 'error', last_error: 'harness exited with code 137 (out of memory)', branch: 'feat/build-cache', created_ms: -140000000, updated_ms: -139000000 }),
  session('a0b7a9c8-7547-4819-b658-c7cf5d3c7479', { harness_id: 'external', harness_version: '', harness_agent: 'claude-code', harness_found: null, harness_session_id: 'mcp-7f3a9c', model: 'external', state: 'running', branch: '', repo_path: '', worktree_path: null, tokens_used: 0, budget_tokens: 0, cost_usd: null, created_ms: -600000, updated_ms: -20000 }),
]
const ARCHIVED = [
  session('b1c8b0d9-8658-492a-a769-d8d06e4d858a', { state: 'closed', end_reason: 'item_close', branch: 'docs/readme-refresh', archived_ms: -200000000, created_ms: -260000000, updated_ms: -250000000 }),
  session('c2d9c1ea-9769-4a3b-b87a-e9e17f5e969b', { state: 'closed', end_reason: 'killed_user', branch: 'spike/gitlab-ci', archived_ms: -300000000, created_ms: -360000000, updated_ms: -350000000 }),
  session('d3eadafb-a87a-4b4c-c98b-faf2806fa7ac', { state: 'closed', end_reason: 'phase_done', harness_id: 'claude-acp', harness_version: '0.9.2', branch: 'feat/early-gateway', archived_ms: -900000000, legacy_ms: -900000000, created_ms: -1200000000, updated_ms: -1190000000 }),
]

const LONG_BRANCH = 'feat/a-really-quite-long-branch-name-describing-per-channel-rate-limits-with-retry-queue-caps-and-ceiling-window-reuse'
const LONG_ERROR =
  'rpc: rpc error -32603: harness stream ended while a tool call was in flight: /var/lib/tracon/worktrees/orbit-really-long-path-segment-that-goes-on/target/debug/build/ring-0b5a3e2f1c9d8e7a/out/libring_core_0_17_8_.a: No space left on device (os error 28)'
const many = Array.from({ length: 28 }, (_, i) =>
  session(`e${String(i).padStart(2, '0')}0e1f2-3a4b-4c5d-8e6f-${String(i).padStart(12, '0')}`, {
    state: i % 7 === 0 ? 'failed' : 'closed',
    end_reason: i % 7 === 0 ? 'error' : ['item_close', 'phase_done', 'killed_user', 'node_restart', 'provider_exhausted', 'continued'][i % 6],
    last_error: i % 7 === 0 ? LONG_ERROR : null,
    branch: i % 3 === 0 ? `${LONG_BRANCH}-${i}` : `fix/issue-${400 + i}`,
    repo_path: i % 4 === 0 ? '/home/op/src/clients/acme-industries/monorepo-with-a-long-name/services/billing-reconciliation' : '/home/op/src/orbit',
    model: i % 5 === 0 ? 'anthropic/claude-opus-4-1-20250805-with-a-long-model-identifier' : 'sonnet',
    channel: i % 2 ? 'personal' : 'client-acme-industries-long-channel',
    created_ms: -(i + 2) * 7200000,
    updated_ms: -(i + 2) * 7200000 + 3600000,
  }),
)

// ---- session screens -------------------------------------------------------

const S = {
  starting: session('a1f0c3e2-7b4d-4e19-9c58-2d6b0f8e1a73', { state: 'starting', harness_found: null, harness_agent: null, harness_protocol: null, tokens_used: 0, cost_usd: 0, context_used: null, context_size: null, branch: 'fix/egress-retry', created_ms: -40000, updated_ms: -8000 }),
  working: session('b2e1d4f3-8c5e-4f2a-ad69-3e7c1a9f2b84', { state: 'running', turn_active: 1, harness_id: 'opencode', harness_version: '1.3.4', harness_agent: 'opencode', harness_found: '1.3.4', harness_protocol: 'opencode-v1', model: 'anthropic/claude-sonnet-4-5', branch: 'feat/service-sidecars', created_ms: -1800000, updated_ms: -4000 }),
  waiting: session('c3f2e5a4-9d6f-4a3b-be7a-4f8d2b0a3c95', { state: 'waiting_on_you', updated_ms: -95000 }),
  question: session('c4a3e6b5-0d7f-4b4c-9f8b-5b0e4c2d5fa6', { state: 'waiting_on_you', branch: 'feat/repo-entry', updated_ms: -300000 }),
  check: session('d4a3f6b5-ae7a-4b4c-8f8b-5a9e3c1b4da6', { state: 'waiting_on_check', branch: 'feat/outcome-record', created_ms: -5400000, updated_ms: -61000 }),
  idle: session('d5b4a7c6-bf8b-4c5d-9a9c-6b0f4d2c5eb7', { state: 'running', turn_active: 0, branch: 'feat/rate-limits' }),
  paused: session('e5b4a7c6-bf8b-4c5d-9a9c-6b0f4d2c5eb7', { state: 'paused', branch: 'chore/bump-axum', model: 'opus', tokens_used: 1310000, created_ms: -9000000, updated_ms: -3000000 }),
  exhaustedWait: session('e6c5b8d7-c09c-4d6e-8bad-7c1a5e3d6fc9', { state: 'paused', branch: 'feat/exhaustion-policy', model: 'opus', tokens_used: 1480000, updated_ms: -900000 }),
  suspended: session('f6c5b8d7-ca9c-4d6e-8bad-7c1a5e3d6fc8', { state: 'suspended', branch: 'fix/review-draft-race', created_ms: -14400000, updated_ms: -7200000, context_used: null, context_size: null }),
  restart: session('17e8d0f9-ecbe-4f80-adcf-9e3c7a5f8be0', { state: 'closed', end_reason: 'node_restart', branch: 'feat/awake-inhibitor', created_ms: -20000000, updated_ms: -17000000, draft: 'Also hold the inhibitor while a permission is waiting — the card should not expire because the lid closed.' }),
  exhaustedEnd: session('28f9e1a0-fdcf-4091-bed0-af4d8b6a9cf1', { state: 'closed', end_reason: 'provider_exhausted', branch: 'feat/exhaustion-policy', created_ms: -26000000, updated_ms: -24000000 }),
  done: session('4ab1a3c2-1fe1-42b3-90f2-c16fad8c1e13', { state: 'closed', end_reason: 'item_close', branch: 'feat/rate-limits', created_ms: -93600000, updated_ms: -90000000, review_id: null }),
  plan: session('5bc2b4d3-20f2-43c4-a103-d27a0e9d2f24', { state: 'closed', end_reason: 'phase_done', phase: 'plan', branch: 'plan/repo-entry', created_ms: -100000000, updated_ms: -99000000 }),
  killed: session('7de4d6f5-4214-45e6-8325-f49c2a0f4146', { state: 'closed', end_reason: 'killed_user', branch: 'spike/pty-capture', created_ms: -120000000, updated_ms: -118000000 }),
  budget: session('8ef5e7a6-5325-46f7-9436-a5ad3b1a5257', { state: 'killed_budget', tokens_used: 2004112, branch: 'refactor/store-split', created_ms: -130000000, updated_ms: -125000000 }),
  failed: session('9fa6f8b7-6436-4708-a547-b6be4c2b6368', { state: 'failed', end_reason: 'error', last_error: 'harness exited with code 137 (out of memory)', branch: 'feat/build-cache', created_ms: -140000000, updated_ms: -139000000 }),
  external: session('a0b7a9c8-7547-4819-b658-c7cf5d3c7479', { harness_id: 'external', harness_version: '', harness_agent: 'claude-code', harness_found: null, harness_protocol: null, harness_session_id: 'mcp-7f3a9c', model: 'external', state: 'running', branch: '', repo_path: '', worktree_path: null, tokens_used: 0, budget_tokens: 0, cost_usd: null, context_used: null, context_size: null, policy_version: 12, manifest_digest: null, work_item_id: null, created_ms: -600000, updated_ms: -20000 }),
  legacy: session('d3eadafb-a87a-4b4c-c98b-faf2806fa7ac', { state: 'closed', end_reason: 'phase_done', harness_id: 'claude-acp', harness_version: '0.9.2', harness_found: '0.9.2', harness_agent: 'claude-code-acp', branch: 'feat/early-gateway', archived_ms: -900000000, legacy_ms: -900000000, created_ms: -1200000000, updated_ms: -1190000000 }),
  remote: session('a7d6c9e8-dbad-4e7f-9cbe-8d2b6f4e7ad9', { node_id: PEER, channel: 'work', state: 'running', branch: 'feat/queue-metrics', repo_path: '/srv/repos/platform/orbit', created_ms: -3600000, updated_ms: -1900000 }),
  transcript: session('b8c7d0e9-1f2a-4b3c-8d4e-5f6a7b8c9d0e', { state: 'running', turn_active: 1, branch: 'feat/rate-limits', created_ms: -900000, updated_ms: -3000 }),
  long: session('fa0e1d2c-3b4a-4596-8778-695a4b3c2d1e', {
    state: 'running',
    branch: LONG_BRANCH,
    worktree_path: '/var/lib/tracon/worktrees/clients-acme-industries-monorepo-with-a-long-name-services-billing-reconciliation-fa0e1d',
    repo_path: '/home/op/src/clients/acme-industries/monorepo-with-a-long-name',
    model: 'anthropic/claude-opus-4-1-20250805-with-a-long-model-identifier',
    channel: 'client-acme-industries-long-channel',
    tokens_used: 1876000,
    cost_usd: 143.27,
    context_used: 191000,
  }),
}
const CONTINUED = session('0b1c2d3e-4f50-4617-8829-3a4b5c6d7e8f', { state: 'running', turn_active: 1, continued_from: S.restart.id, branch: S.restart.branch, created_ms: -900000, updated_ms: -10000, tokens_used: 61000 })
const RESUMED_FROM_SUSPEND = session('1c2d3e4f-5061-4728-9930-4b5c6d7e8f90', { state: 'running', continued_from: S.suspended.id, branch: S.suspended.branch, created_ms: -600000, updated_ms: -20000, tokens_used: 22000 })

const exhaustionWaiting = {
  policy: 'fallback_then_wait',
  fallback: null,
  provider: 'anthropic',
  model: 'claude-opus-4-1',
  reason: 'weekly usage limit reached for this subscription',
  reset_ms: 9000000,
  next_wake_ms: 9000000,
  boundary_seq: 42,
  outcome: 'waiting',
  note: null,
  continued_by: null,
}

const publishedEvents = (sid) => [
  ...opening(sid, { branch: 'fix/review-draft-race', prompt: 'Two tabs saving the review draft can overwrite each other. Make the second save see a 409 with the newer copy.' }),
  ...workTurn(sid, -14000000),
  ev(sid, 'check_started', { commands: ['just check', 'just test'] }, -13000000),
  ev(sid, 'check_result', { command: 'just check', ok: true, exit: 0, tail: 'ok', ms: 94000 }, -12900000),
  ev(sid, 'check_result', { command: 'just test', ok: true, exit: 0, tail: 'test result: ok. 412 passed; 0 failed', ms: 188000 }, -12700000),
  ev(sid, 'published', { url: 'https://github.com/cosmic-example/orbit/pull/418', review_id: 'rev-9a8b7c6d' }, -9000000),
  ev(sid, 'forge_follow', { changes: ['CI passed', 'review requested'], url: 'https://github.com/cosmic-example/orbit/pull/418' }, -8000000),
  ev(sid, 'session_suspended', { idle_ms: 1800000 }, -7200000),
  ev(sid, 'state', { state: 'suspended' }, -7200000),
]

export default [
  // ---------------- list ----------------
  {
    id: 'sessions-list',
    area: 'sessions',
    route: '/sessions',
    title: 'Sessions list, every lifecycle state',
    since: '#375',
    note: 'One row per kind: starting, working (OpenCode), waiting on you, on a check, paused, suspended · published (#380), a peer\'s, ended by node restart (#375), provider exhausted (#384), continued, item closed, planned, reviewed, killed, budget, failed, external. Archived and Legacy collapsed below.',
    api: {
      'GET /api/sessions': [...ROWS, ...ARCHIVED],
      'GET /api/queue': NO_QUEUE,
    },
  },
  {
    id: 'sessions-list-archived-open',
    area: 'sessions',
    route: '/sessions',
    title: 'Sessions list, archived and legacy expanded',
    note: 'The archived row keeps an unarchive button; the legacy row has none.',
    api: { 'GET /api/sessions': [...ROWS.slice(10), ...ARCHIVED], 'GET /api/queue': NO_QUEUE },
    act: async (page) => {
      const show = page.getByRole('button', { name: 'show', exact: true })
      while ((await show.count()) > 0) await show.first().click()
    },
  },
  {
    id: 'sessions-list-empty',
    area: 'sessions',
    route: '/sessions',
    title: 'No sessions at all',
    api: { 'GET /api/sessions': [], 'GET /api/queue': NO_QUEUE },
  },
  {
    id: 'sessions-list-long',
    area: 'sessions',
    route: '/sessions',
    title: 'Many rows, long branches, paths, models and errors',
    note: 'Ellipsis on branch and the small line; row height should stay even. Failed rows carry a long error.',
    api: { 'GET /api/sessions': [S.long, ...many], 'GET /api/queue': NO_QUEUE },
  },
  {
    id: 'sessions-list-stale-peer',
    area: 'sessions',
    route: '/sessions',
    title: 'A peer node unreachable: its rows dim',
    note: 'work-pod not heard from for 40m: its running row dims and says last seen.',
    api: {
      'GET /api/sessions': [S.remote, S.waiting, session('b8c9d0e1-2f3a-4b5c-8d6e-7f8091a2b3c4', { node_id: PEER, channel: 'work', state: 'closed', end_reason: 'item_close', branch: 'fix/flaky-ci', created_ms: -9000000, updated_ms: -8000000 })],
      'GET /api/queue': NO_QUEUE,
      'GET /api/nodes': exact([
        { id: SELF, name: 'laptop', state: 'ready', failed_check: null, failed_detail: null, harness: { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false }, models: [{ value: 'sonnet', name: 'Sonnet' }], checked_at_ms: -420000, is_self: true, reachable: true, last_seen_ms: null, providers: [] },
        { id: PEER, name: 'work-pod', state: 'ready', failed_check: null, failed_detail: null, harness: { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false }, models: [{ value: 'sonnet', name: 'Sonnet' }], checked_at_ms: -2400000, is_self: false, reachable: false, last_seen_ms: -2400000, providers: [] },
      ]),
    },
  },
  {
    id: 'sessions-list-error',
    area: 'sessions',
    route: '/sessions',
    title: 'Session list request fails (500)',
    note: 'The error with a Retry, and neither "No sessions running" nor "Nothing has ended yet".',
    api: { 'GET /api/sessions': fail(500, 'database is locked'), 'GET /api/queue': NO_QUEUE },
  },
  {
    id: 'sessions-list-retried',
    area: 'sessions',
    route: '/sessions',
    title: 'Session list failed once, then Retry',
    note: 'The first snapshot fails; Retry refetches and the rows replace the error.',
    api: { 'GET /api/sessions': ROWS.slice(0, 6), 'GET /api/queue': NO_QUEUE },
    init: flaky('/api/sessions', [1]),
    act: async (page) => {
      await page.getByRole('button', { name: 'Retry' }).click()
      await page.locator('.h4', { hasText: 'Ended' }).first().waitFor()
    },
  },
  {
    id: 'sessions-refresh-error',
    area: 'sessions',
    route: '/sessions',
    title: 'Session list loaded, then a refetch failed',
    note: 'The stream reconnects and the refetch of /api/sessions fails: the rows stay, under "Could not refresh sessions" with a Retry.',
    api: { 'GET /api/sessions': ROWS.slice(0, 6), 'GET /api/queue': NO_QUEUE },
    init: flaky('/api/sessions', [2]),
    act: async (page) => {
      await page.locator('.h4', { hasText: 'Ended' }).first().waitFor()
      await streamEvent(page, 'open')
      await page.getByText('Could not refresh sessions').waitFor()
    },
  },
  {
    id: 'sessions-list-loading',
    area: 'sessions',
    route: '/sessions',
    title: 'Session list still loading',
    note: 'The list request never settles.',
    api: { 'GET /api/sessions': never, 'GET /api/queue': NO_QUEUE },
  },

  // ---------------- a session, live ----------------
  {
    id: 'sessions-starting',
    area: 'sessions',
    route: `/sessions/${S.starting.id}`,
    title: 'Starting: building the repository image',
    note: 'Pause is hidden while starting; Stop stays. Input disabled "starting". The repo image line (#390 cache).',
    api: page(S.starting, {
      events: [
        ev(S.starting.id, 'worktree', { path: S.starting.worktree_path, branch: 'fix/egress-retry', base: 'origin/main', main_checkout_dirty: true }, -38000),
        ev(S.starting.id, 'repo_image', { status: 'building', repo: '/home/op/src/orbit' }, -36000),
      ],
      detail: { usage: { ...usageReconciled, state: null, gateway_tokens: 0, harness_tokens: 0, charged_tokens: 0, cache_read_tokens: 0 } },
    }),
  },
  {
    id: 'sessions-running-working',
    area: 'sessions',
    route: `/sessions/${S.working.id}`,
    title: 'Running, a turn in progress (OpenCode)',
    since: '#389',
    note: 'Open tool group with a call still running; service sidecar events (#389) and policy_allowed lines; keeping-awake note in the rail foot (#379); OpenCode link in the header; lsp/fmt note.',
    api: page(S.working, {
      events: [
        ...opening(S.working.id, { model: 'anthropic/claude-sonnet-4-5', branch: 'feat/service-sidecars', prompt: 'Start postgres beside the session by name from the catalogue and run the migration tests against it.' }),
        ev(S.working.id, 'service', { name: 'postgres', container: 'tracon-svc-b2e1d4-postgres', state: 'started' }, -1700000),
        ev(S.working.id, 'service', { name: 'postgres', container: 'tracon-svc-b2e1d4-postgres', state: 'ready' }, -1690000),
        ev(S.working.id, 'message', { text: 'Postgres is up as `postgres:5432` on the session network. Running the migration tests against it now.' }, -1600000),
        ev(S.working.id, 'tool_call', { title: 'bash: cargo test -p tracon-node --test migrations', kind: 'execute' }, -1500000, 'call_m1'),
        ev(S.working.id, 'policy_allowed', { title: 'bash: cargo test -p tracon-node --test migrations', action: 'bash', kind: 'execute', resource: null, command: 'cargo test -p tracon-node --test migrations', rule: 'cargo-local', reason: 'Local builds and tests.', tool_call_id: 'call_m1' }, -1499000),
        ev(S.working.id, 'tool_result', { status: 'completed', truncated: true }, -1300000, 'call_m1'),
        ev(S.working.id, 'tool_call', { title: 'read node/src/sidecars.rs', kind: 'read' }, -30000, 'call_r1'),
        ev(S.working.id, 'tool_result', { status: 'completed' }, -29000, 'call_r1'),
        ev(S.working.id, 'tool_call', { title: 'edit node/src/sidecars.rs', kind: 'edit' }, -20000, 'call_e1'),
        ev(S.working.id, 'tool_result', { status: 'completed' }, -19000, 'call_e1'),
        ev(S.working.id, 'tool_call', { title: 'bash: cargo test -p tracon-node --test sidecars', kind: 'execute' }, -9000, 'call_t1'),
      ],
      api: {
        'GET /api/awake': { held: true, reason: '1 session working', method: 'logind', error: null, last_suspend: null },
      },
    }),
  },
  {
    id: 'sessions-transcript',
    area: 'sessions',
    route: `/sessions/${S.transcript.id}`,
    title: 'Tool calls as a transcript (Claude Code)',
    note: 'A finished run folded with its duration and its failed build named under it; the open run below has a call still running. Paths come from raw_input, from the worktree.',
    api: page(S.transcript, { events: transcriptEvents(S.transcript.id) }),
  },
  {
    id: 'sessions-transcript-opened',
    area: 'sessions',
    route: `/sessions/${S.transcript.id}`,
    title: 'Tool calls as a transcript, every run opened',
    note: 'After pressing o: every run open, three reads under one head, the failed build opened to its output.',
    api: page(S.transcript, { events: transcriptEvents(S.transcript.id) }),
    act: async (page) => {
      await page.keyboard.press('o')
      await page.locator('details.call.reads > summary').first().click()
      await page.locator('details.call.crit > summary').first().click()
    },
  },
  {
    id: 'sessions-waiting-permission',
    area: 'sessions',
    route: `/sessions/${S.waiting.id}`,
    title: 'Waiting on a permission, with a draft the node kept',
    note: 'Inline permission card with a session-grant option and the intent line; "draft restored" banner and the draft in the composer.',
    api: page(S.waiting, {
      draft: 'Also cap the retry queue — the same window, not a new table.',
      events: [
        ...opening(S.waiting.id),
        ...workTurn(S.waiting.id),
        ev(S.waiting.id, 'user_prompt', { text: "Good. Keep the refusal message in the same voice as the ceiling's." }, -900000),
        ev(S.waiting.id, 'message', { text: 'Done — refusals now read like the ceiling\'s. Running the gateway tests before submitting; I need permission for `cargo test`.' }, -100000),
        ev(S.waiting.id, 'permission_request', { title: 'Bash: cargo test -p tracon-node --test gateway' }, -95000, 'perm-41'),
      ],
      queue: { ...NO_QUEUE, waiting: [perm(S.waiting.id)] },
    }),
  },
  {
    id: 'sessions-waiting-egress',
    area: 'sessions',
    route: `/sessions/${S.waiting.id}`,
    title: 'Waiting on an egress request',
    note: 'Egress card: four options, nothing to edit; the log line for the refused host.',
    api: page(S.waiting, {
      events: [
        ...opening(S.waiting.id),
        ev(S.waiting.id, 'egress_refused', { host: 'files.pythonhosted.org', approval_id: 'perm-42' }, -60000),
        ev(S.waiting.id, 'permission_request', { title: 'Reach files.pythonhosted.org' }, -59000, 'perm-42'),
      ],
      queue: {
        ...NO_QUEUE,
        waiting: [
          perm(S.waiting.id, {
            id: 'perm-42',
            title: 'Reach files.pythonhosted.org',
            kind: 'fetch',
            raw_input: JSON.stringify({ tool: 'request_egress', host: 'files.pythonhosted.org', reason: 'pip install for the docs build' }),
            options: JSON.stringify([
              { option_id: 'allow_once', name: 'Allow once', kind: 'allow_once' },
              { option_id: 'allow_session', name: 'For this session', kind: 'allow_session' },
              { option_id: 'allow_repo', name: "Save to the repository's egress", kind: 'allow_repo' },
              { option_id: 'reject_once', name: 'Reject', kind: 'reject_once' },
            ]),
          }),
        ],
      },
    }),
  },
  {
    id: 'sessions-waiting-operator',
    area: 'sessions',
    route: `/sessions/${S.question.id}`,
    title: 'Waiting on the operator: a question with choices',
    since: '#391',
    note: 'An agent drafting a repository entry (#391) asks which check command to propose. Question card below the log.',
    api: page(S.question, {
      events: [
        ...opening(S.question.id, { branch: 'feat/repo-entry', prompt: 'Draft this repository\'s entry: image, checks, egress. Try it, then propose it.' }),
        ev(S.question.id, 'tool_call', { title: 'repo_setup_draft', kind: 'other' }, -900000, 'toolu_rs1'),
        ev(S.question.id, 'tool_result', { status: 'completed' }, -899000, 'toolu_rs1'),
        ev(S.question.id, 'tool_call', { title: 'repo_setup_try', kind: 'other' }, -800000, 'toolu_rs2'),
        ev(S.question.id, 'tool_result', { status: 'completed' }, -620000, 'toolu_rs2'),
        ev(S.question.id, 'message', { text: 'The draft builds and both checks run in the image. Before proposing it I need to know which test command you want required.' }, -310000),
        ev(S.question.id, 'tool_call', { title: 'ask_operator', kind: 'other' }, -300000, 'toolu_ask'),
      ],
      detail: {
        questions: [
          {
            id: 'q-3',
            session_id: S.question.id,
            channel: 'personal',
            node_id: SELF,
            prompt: 'Which command should the repository entry require before a review can publish? `just test` runs the whole workspace (≈4 min); `cargo nextest run -p tracon-node` covers the node only (≈90 s).',
            choices_json: JSON.stringify(['just test', 'cargo nextest run -p tracon-node', 'both']),
            request_key: null,
            state: 'unanswered',
            answer_json: null,
            created_ms: -300000,
            answered_ms: null,
          },
        ],
      },
    }),
  },
  {
    id: 'sessions-waiting-check',
    area: 'sessions',
    route: `/sessions/${S.check.id}`,
    title: 'Running the required checks',
    note: 'Banner names the running command and elapsed; first check failed and is expanded; composer disabled.',
    api: page(S.check, {
      events: [
        ...opening(S.check.id, { branch: 'feat/outcome-record' }),
        ...workTurn(S.check.id, -5000000),
        ev(S.check.id, 'check_started', { commands: ['just check', 'just test'] }, -240000),
        ev(S.check.id, 'check_prepared', { ok: true, ms: 41000 }, -199000),
        ev(S.check.id, 'check_result', { command: 'just check', ok: false, exit: 1, tail: 'error: unused import: `OutcomeClaim`\n --> node/src/outcome.rs:14:5\n   |\n14 | use crate::outcome::OutcomeClaim;\n   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^\n\nerror: could not compile `tracon-node` (lib) due to 1 previous error', ms: 58000 }, -140000),
      ],
    }),
  },
  {
    id: 'sessions-check-failed-test',
    area: 'sessions',
    route: `/sessions/${S.check.id}`,
    title: 'A check failed on a test',
    note: 'The failed check leads with what failed (test name, panic, verdict), read from the whole output; the tail is only compiler progress.',
    api: page(S.check, {
      events: [
        ...opening(S.check.id, { branch: 'feat/outcome-record' }),
        ...workTurn(S.check.id, -5000000),
        ev(S.check.id, 'check_started', { commands: ['just check'] }, -400000),
        ev(S.check.id, 'check_prepared', { ok: true, ms: 41000 }, -359000),
        ev(
          S.check.id,
          'check_result',
          {
            command: 'just check',
            ok: false,
            exit: 101,
            failures:
              "test review::output::tests::refills_to_capacity ... FAILED\nthread 'review::output::tests::refills_to_capacity' (903584) panicked at node/src/review/output.rs:612:40:\nassertion `left == right` failed: bucket overfilled\nleft: 4\nright: 5\nfailures:\nreview::output::tests::refills_to_capacity\ntest result: FAILED. 1203 passed; 1 failed; 4 ignored; 0 measured; 0 filtered out; finished in 41.20s\nerror: test failed, to rerun pass `-p tracon --lib`\nerror: recipe `check` failed on line 14 with exit code 101",
            tail:
              '…   Compiling tracon-proto v0.29.0 (/work/proto)\n   Compiling tracon-mesh v0.29.0 (/work/mesh)\n   Compiling tracon v0.29.0 (/work/node)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 3m 12s\n     Running unittests src/lib.rs (target/debug/deps/tracon-6f1c0e2a9b7d4c31)\nerror: test failed, to rerun pass `-p tracon --lib`\nerror: recipe `check` failed on line 14 with exit code 101',
            ms: 251000,
          },
          -100000,
        ),
      ],
    }),
  },
  {
    id: 'sessions-idle',
    area: 'sessions',
    route: `/sessions/${S.idle.id}`,
    title: 'Idle between turns, usage mismatched',
    note: 'Composer live; usage note in amber (mismatched turn); usage_mismatch and host_suspended lines (#379) in the log; rail foot shows "slept N min".',
    api: page(S.idle, {
      events: exact([
        ...opening(S.idle.id),
        ...workTurn(S.idle.id),
        ev(S.idle.id, 'usage_mismatch', { turn: 1, gateway: { tokens: 84213, requests: 6 }, harness: { tokens: 61002 }, charged_tokens: 84213 }, -2499000),
        ev(S.idle.id, 'host_suspended', { asleep_ms: 2280000, turn_active: true, inhibitor_held: false }, -1200000),
        ev(S.idle.id, 'message', { text: 'Both files are changed and the gateway tests pass locally. Ready for the next instruction.' }, -150000),
        ev(S.idle.id, 'turn_end', { usage: { total_tokens: 31840 } }, -140000),
      ]),
      detail: { usage: { ...usageReconciled, state: 'mismatch', harness_tokens: 361000, mismatched_turns: 1 } },
      api: {
        'GET /api/awake': exact({ held: false, reason: null, method: 'logind', error: null, last_suspend: { woke_ms: -1200000, asleep_ms: 2280000 } }),
      },
    }),
  },
  {
    id: 'sessions-send-refused',
    area: 'sessions',
    route: `/sessions/${S.idle.id}`,
    title: 'Sending a prompt is refused (409)',
    note: '"refused" banner under the panels after Send.',
    api: page(S.idle, {
      events: [...opening(S.idle.id), ...workTurn(S.idle.id)],
      api: {
        [`POST /api/sessions/${S.idle.id}/prompt`]: fail(409, 'a prompt to this session may already be running: the last dispatch never reported, so a second one is refused until it settles'),
      },
    }),
    act: async (page) => {
      await page.locator('form.prompt textarea').fill('Now cap the retry queue with the same window.')
      await page.getByRole('button', { name: 'Send' }).click()
    },
  },
  {
    id: 'sessions-paused',
    area: 'sessions',
    route: `/sessions/${S.paused.id}`,
    title: 'Paused by the operator',
    note: 'Resume in the header; banner explains the fence; repetition line in the log.',
    api: page(S.paused, {
      events: [
        ...opening(S.paused.id, { model: 'opus', branch: 'chore/bump-axum' }),
        ev(S.paused.id, 'tool_call', { title: 'Bash: cargo update -p axum', kind: 'execute' }, -3300000, 'toolu_p1'),
        ev(S.paused.id, 'tool_result', { status: 'failed' }, -3290000, 'toolu_p1'),
        ev(S.paused.id, 'repetition', { what: 'tool_call', count: 4, title: 'Bash: cargo update -p axum', kind: 'execute' }, -3100000),
        ev(S.paused.id, 'session_paused', { source: 'operator', reason: 'operator paused the session' }, -3000000),
        ev(S.paused.id, 'state', { state: 'paused' }, -3000000),
      ],
    }),
  },
  {
    id: 'sessions-log-every-kind',
    area: 'sessions',
    route: `/sessions/${S.idle.id}`,
    title: 'Log: one of each event kind with a plain line',
    note: 'Every kind the log renders through eventLine: none may read as a bare identifier.',
    api: page(S.idle, {
      events: [
        ...opening(S.idle.id, { model: 'opus', branch: 'feat/rate-limits' }),
        ev(S.idle.id, 'service', { name: 'postgres', container: 'tracon-svc-d5b4a7-postgres', state: 'started', image: 'postgres:17' }, -2600000),
        ev(S.idle.id, 'service', { name: 'postgres', container: 'tracon-svc-d5b4a7-postgres', state: 'ready' }, -2590000),
        ev(S.idle.id, 'service', { name: 'redis', container: 'tracon-svc-d5b4a7-redis', state: 'failed', detail: 'did not answer on 6379 within 30s' }, -2580000),
        ev(S.idle.id, 'pty_opened', { gateway: 'opencode', phase: 'spawn', pty_id: 'pty_1', command: 'bash', args: ['-l'], cwd: '/workspace', env_kept: 4, env_dropped: 2 }, -2500000),
        ev(S.idle.id, 'pty_closed', { gateway: 'opencode', phase: 'removed', pty_id: 'pty_1', duration_ms: 95000, reason: 'the terminal was closed through the gateway' }, -2400000),
        ev(S.idle.id, 'child_session', { child: 'ses_7f3a9c', parent: 'ses_1b2c3d', untracked: true }, -2300000),
        ev(S.idle.id, 'uncertain', { reason: 'the prompt was dispatched and never reported', intent: 'int-4', refusing: 'prompt' }, -2200000),
        ev(S.idle.id, 'uncertain', { cleared: 'the turn it started ended' }, -2190000),
        ev(S.idle.id, 'session_paused', { source: 'watchdog', reason: '3 failed tool calls in a row: Bash: cargo test' }, -2100000),
        ev(S.idle.id, 'session_resumed', { source: 'operator', reason: 'operator resumed the session' }, -2000000),
        ev(S.idle.id, 'approval_settled', { approval_id: 'perm-9', tool: 'pr_merge', state: 'succeeded', reason: null }, -1900000),
        ev(S.idle.id, 'approval_settled', { approval_id: 'perm-10', tool: 'run_rerun', state: 'failed', reason: 'the run is still in progress' }, -1890000),
        ev(S.idle.id, 'candidate_verified', { candidate_id: 'cand-3', head_sha: '8c4be91f03a2d6e7b5c1f0a9e3d4b2c7a6f5e1d0', reused: false }, -1800000),
        ev(S.idle.id, 'review_decision', { source: 'operator', review_id: 'rev-7', decision: 'revise', waiting_ms: 540000 }, -1700000),
        ev(S.idle.id, 'review_decision', { source: 'operator', review_id: 'rev-7', decision: 'approved', waiting_ms: 120000 }, -1600000),
        ev(S.idle.id, 'published', { url: 'https://github.com/cosmic-example/orbit/pull/412', review_id: 'rev-7' }, -1590000),
        ev(S.idle.id, 'provider_exhausted', { policy: 'fallback', outcome: 'falling_back', provider: 'anthropic', model: 'claude-opus-4-1', status: 429, reason: 'weekly usage limit reached', reset_ms: null, next_wake_ms: null, fallback: 'openai/gpt-5', note: null }, -1500000),
        ev(S.idle.id, 'session_paused', { source: 'exhausted', reason: 'anthropic is exhausted (weekly usage limit reached); it continues on openai/gpt-5.' }, -1499000),
        ev(S.idle.id, 'exhaustion_boundary', { boundary_seq: 61, outcome: 'falling_back' }, -1490000),
        ev(S.idle.id, 'exhaustion_wake', { outcome: 'continued', continued_by: CONTINUED.id, model: 'openai/gpt-5' }, -1480000),
        ev(S.idle.id, 'exhaustion_wake', { outcome: 'held', note: 'The node did not carry on: openai/gpt-5 is not bound to this channel.' }, -1470000),
        ev(S.idle.id, 'session_suspended', { idle_ms: 1800000 }, -1400000),
        ev(S.idle.id, 'abandoned', { reason: 'item closed', summary: 'superseded by the gateway rewrite' }, -1300000),
        ev(S.idle.id, 'some_future_kind', { name: 'example', count: 2 }, -1200000),
      ],
    }),
  },
  {
    id: 'sessions-exhausted-waiting',
    area: 'sessions',
    route: `/sessions/${S.exhaustedWait.id}`,
    title: 'Provider exhausted: waiting for the limit to reset',
    since: '#384',
    note: 'Paused banner says which provider and when it resumes. Log shows provider_error then the exhaustion events.',
    api: page(S.exhaustedWait, {
      events: [
        ...opening(S.exhaustedWait.id, { model: 'opus', branch: 'feat/exhaustion-policy' }),
        ...workTurn(S.exhaustedWait.id, -2000000),
        ev(S.exhaustedWait.id, 'provider_error', { provider: 'anthropic', status: 429, message: 'rate_limit_error', attempt: 2, cause: 'exhausted' }, -910000),
        ev(S.exhaustedWait.id, 'provider_exhausted', { policy: 'fallback_then_wait', outcome: 'waiting', provider: 'anthropic', model: 'claude-opus-4-1', status: 429, reason: 'weekly usage limit reached for this subscription', reset_ms: 9000000, next_wake_ms: 9000000, fallback: null, note: null }, -900000),
        ev(S.exhaustedWait.id, 'session_paused', { source: 'exhausted', reason: 'anthropic is exhausted (weekly usage limit reached for this subscription); it resumes when the provider\'s limit resets at 2026-10-08 18:00 UTC.' }, -899000),
        ev(S.exhaustedWait.id, 'state', { state: 'paused' }, -899000),
        ev(S.exhaustedWait.id, 'exhaustion_boundary', { boundary_seq: 42, outcome: 'waiting' }, -890000),
      ],
      detail: { exhaustion: exhaustionWaiting },
    }),
  },
  {
    id: 'sessions-exhausted-held',
    area: 'sessions',
    route: `/sessions/${S.exhaustedWait.id}`,
    title: 'Provider exhausted: held for the operator',
    since: '#384',
    note: 'outcome held with the node\'s note; no reset time from the provider.',
    api: page(S.exhaustedWait, {
      events: [
        ...opening(S.exhaustedWait.id, { model: 'opus', branch: 'feat/exhaustion-policy' }),
        ev(S.exhaustedWait.id, 'provider_exhausted', { policy: 'pause', outcome: 'held', provider: 'anthropic', model: 'claude-opus-4-1', status: 429, reason: 'subscription usage limit reached; the provider sent no reset time', reset_ms: null, next_wake_ms: null, fallback: null, note: 'no reset time was given and this session has no fallback; resume it once the limit lifts, or continue it on another model' }, -900000),
        ev(S.exhaustedWait.id, 'session_paused', { source: 'exhausted', reason: 'anthropic is exhausted (subscription usage limit reached); it waits for you.' }, -899000),
        ev(S.exhaustedWait.id, 'state', { state: 'paused' }, -899000),
        ev(S.exhaustedWait.id, 'exhaustion_boundary', { boundary_seq: 42, outcome: 'held' }, -890000),
      ],
      detail: { exhaustion: { ...exhaustionWaiting, policy: 'pause', reason: 'subscription usage limit reached; the provider sent no reset time', reset_ms: null, next_wake_ms: null, outcome: 'held', note: 'no reset time was given and this session has no fallback; resume it once the limit lifts, or continue it on another model' } },
    }),
  },
  {
    id: 'sessions-exhausted-falling-back',
    area: 'sessions',
    route: `/sessions/${S.exhaustedWait.id}`,
    title: 'Provider exhausted: falling back to another model',
    since: '#384',
    api: page(S.exhaustedWait, {
      events: [...opening(S.exhaustedWait.id, { model: 'opus', branch: 'feat/exhaustion-policy' })],
      detail: { exhaustion: { ...exhaustionWaiting, policy: 'fallback', fallback: 'openai/gpt-5', outcome: 'falling_back' } },
    }),
  },

  // ---------------- suspended / published ----------------
  {
    id: 'sessions-suspended',
    area: 'sessions',
    route: `/sessions/${S.suspended.id}`,
    title: 'Suspended after publishing',
    since: '#380',
    note: 'Published banner (forge URL + review), suspended banner with Continue, published / session_suspended events in the log, input disabled.',
    api: page(S.suspended, {
      events: publishedEvents(S.suspended.id),
      detail: { publications: [{ review_id: 'rev-9a8b7c6d', url: 'https://github.com/cosmic-example/orbit/pull/418' }] },
    }),
  },
  {
    id: 'sessions-suspended-continued',
    area: 'sessions',
    route: `/sessions/${S.suspended.id}`,
    title: 'Suspended, already continued',
    since: '#380',
    note: 'The Continue button gives way to "continued as …"; two publications stack.',
    api: page(S.suspended, {
      list: [S.suspended, RESUMED_FROM_SUSPEND],
      events: publishedEvents(S.suspended.id),
      detail: {
        publications: [
          { review_id: 'rev-9a8b7c6d', url: 'https://github.com/cosmic-example/orbit/pull/418' },
          { review_id: 'rev-0f1e2d3c', url: 'https://github.com/cosmic-example/orbit-docs/pull/77' },
        ],
      },
    }),
  },

  // ---------------- ended ----------------
  {
    id: 'sessions-node-restart',
    area: 'sessions',
    route: `/sessions/${S.restart.id}`,
    title: 'Ended by a node restart, with Continue',
    since: '#375',
    note: 'Banner: "ended by a node restart · you did not stop it" + Continue. Outcome opens (terminal). Continuation panel and the unsent prompt retained.',
    api: page(S.restart, {
      events: [
        ...opening(S.restart.id, { branch: 'feat/awake-inhibitor', prompt: 'Hold a logind inhibitor while any session works; release it when none does.' }),
        ...workTurn(S.restart.id, -19000000),
        ev(S.restart.id, 'tool_call', { title: 'Bash: cargo test -p tracon-node awake', kind: 'execute' }, -17100000, 'toolu_nr'),
        ev(S.restart.id, 'late_refused', { what: 'tool_result', state: 'closed' }, -16900000),
        ev(S.restart.id, 'state', { state: 'closed' }, -17000000),
      ],
      outcome: outcome(S.restart, { changed: { reviews: [], files: ['node/src/awake.rs'], added: 96, removed: 4, workspace_changes: 3 }, uncertain: ['A tool call was in flight when the node stopped; its result was never recorded.'] }),
      continuation: continuation(S.restart, { next: { kind: 'continue', text: 'The node restarted mid-turn. Carry on from its workspace.', session_id: S.restart.id } }),
    }),
  },
  {
    id: 'sessions-node-restart-continue-refused',
    area: 'sessions',
    route: `/sessions/${S.restart.id}`,
    title: 'Continue after a restart is refused (409)',
    since: '#375',
    note: 'Click Continue in the banner; the node refuses: the refusal shows at the bottom, far from the button.',
    api: page(S.restart, {
      events: [...opening(S.restart.id, { branch: 'feat/awake-inhibitor' }), ev(S.restart.id, 'state', { state: 'closed' }, -17000000)],
      api: { [`POST /api/sessions/${S.restart.id}/continue`]: fail(409, 'this session was already continued once; continue the latest attempt instead') },
    }),
    act: async (page) => {
      await page.locator('.banner button', { hasText: 'Continue' }).click()
    },
  },
  {
    id: 'sessions-continued-from',
    area: 'sessions',
    route: `/sessions/${CONTINUED.id}`,
    title: 'A session continuing a restarted one',
    since: '#375',
    note: '"continues 17e8d0f9 · inherited that one\'s workspace, not its context".',
    api: page(CONTINUED, {
      list: [CONTINUED, { ...S.restart, end_reason: 'continued' }],
      events: [
        ev(CONTINUED.id, 'session_started', { model: 'sonnet', phase: 'execute', policy_version: 12 }, -900000),
        ev(CONTINUED.id, 'user_prompt', { text: 'Continue from where the previous attempt stopped: the node restarted mid-turn while running the awake tests.' }, -880000),
        ev(CONTINUED.id, 'tool_call', { title: 'Bash: cargo test -p tracon-node awake', kind: 'execute' }, -20000, 'toolu_c1'),
      ],
    }),
  },
  {
    id: 'sessions-ended-continued',
    area: 'sessions',
    route: `/sessions/${S.restart.id}`,
    title: 'The restarted session once continued',
    since: '#375',
    note: 'end_reason continued: banner links to the successor.',
    api: page({ ...S.restart, end_reason: 'continued', draft: null }, {
      list: [CONTINUED, { ...S.restart, end_reason: 'continued', draft: null }],
      events: [...opening(S.restart.id, { branch: 'feat/awake-inhibitor' })],
      continuation: continuation(S.restart, {
        attempts: [
          { id: S.restart.id, phase: 'execute', model: 'sonnet', harness: 'claude', state: 'closed', end_reason: 'node_restart', last_error: null, tokens_used: 412000, created_ms: -20000000, continued_from: null, parent_session: null, archived: false },
          { id: CONTINUED.id, phase: 'execute', model: 'sonnet', harness: 'claude', state: 'running', end_reason: null, last_error: null, tokens_used: 61000, created_ms: -900000, continued_from: S.restart.id, parent_session: null, archived: false },
        ],
        next: { kind: 'watch', text: 'The latest attempt is running.', session_id: CONTINUED.id },
        actions: { continue_from: null, abandon: true },
      }),
    }),
  },
  {
    id: 'sessions-ended-provider-exhausted',
    area: 'sessions',
    route: `/sessions/${S.exhaustedEnd.id}`,
    title: 'Ended when its provider was exhausted',
    since: '#384',
    note: 'Banner says which provider and the fallback to carry on with, plus Continue.',
    api: page(S.exhaustedEnd, {
      events: [
        ...opening(S.exhaustedEnd.id, { branch: 'feat/exhaustion-policy' }),
        ev(S.exhaustedEnd.id, 'provider_exhausted', { policy: 'fallback', outcome: 'falling_back', provider: 'anthropic', model: 'claude-sonnet-4-5', status: 429, reason: 'usage window spent', reset_ms: 14000000, next_wake_ms: null, fallback: 'openai/gpt-5', note: null }, -24100000),
        ev(S.exhaustedEnd.id, 'exhaustion_wake', { outcome: 'continued', note: null, continued_by: null }, -24000000),
      ],
      detail: { exhaustion: { ...exhaustionWaiting, policy: 'fallback', fallback: 'openai/gpt-5', model: 'claude-sonnet-4-5', outcome: 'continued', next_wake_ms: null } },
    }),
  },
  {
    id: 'sessions-ended-done',
    area: 'sessions',
    route: `/sessions/${S.done.id}`,
    title: 'Ended normally at item close, outcome populated',
    since: '#386',
    note: 'Outcome panel open: changed review, checks (one from an earlier commit), claims backed / unbacked. Continuation shows done.',
    api: page(S.done, {
      events: [
        ...opening(S.done.id),
        ...workTurn(S.done.id, -93000000),
        ev(S.done.id, 'check_started', { commands: ['just check', 'just test'] }, -91000000),
        ev(S.done.id, 'check_result', { command: 'just check', ok: true, exit: 0, tail: 'ok', ms: 94000 }, -90900000),
        ev(S.done.id, 'check_result', { command: 'just test', ok: true, exit: 0, tail: 'test result: ok. 412 passed', ms: 188000 }, -90700000),
        ev(S.done.id, 'work_shown', { title: 'Rate limit refusal, before and after', head_sha: '8c4be91f03a2', files: 2 }, -90500000),
        ev(S.done.id, 'work_closed', { summary: 'Per-channel limits shipped in #418' }, -90000000),
      ],
      outcome: outcomeDone(S.done),
      continuation: continuation(S.done, {
        next: { kind: 'done', text: 'The work item is closed.', session_id: null },
        actions: { continue_from: null, abandon: false },
        evidence: {
          reviews: [{ id: 'rev-7', session_id: S.done.id, title: 'Per-channel rate limits in the gateway', state: 'published', verdict_reason: null, publish_result: 'https://github.com/cosmic-example/orbit/pull/418', head_sha: '8c4be91', created_ms: -91000000 }],
          shown: [{ id: 'sw-1', session_id: S.done.id, title: 'Rate limit refusal, before and after', head_sha: '8c4be91f03a2', stale: false, created_ms: -90500000 }],
        },
        decisions: { plan: 'plan-rate-limits', brief: null, answered: [{ session_id: S.done.id, kind: 'permission', asked: 'Bash: just test', answer: 'allow_once', at_ms: -91100000 }] },
      }),
    }),
  },
  {
    id: 'sessions-ended-plan',
    area: 'sessions',
    route: `/sessions/${S.plan.id}`,
    title: 'A plan session done',
    api: page(S.plan, {
      events: [
        ...opening(S.plan.id, { phase: 'plan', branch: 'plan/repo-entry', prompt: 'Plan how a session drafts, tries and proposes a repository entry.' }),
        ev(S.plan.id, 'plan_artifact', { channel: 'personal', slug: 'plan-repo-entry-from-session' }, -99000000),
      ],
      outcome: outcome(S.plan),
      continuation: continuation(S.plan, { next: { kind: 'execute', text: 'The plan is written. Start an execute session on it.', session_id: null }, actions: { continue_from: null, abandon: true } }),
    }),
  },
  {
    id: 'sessions-failed',
    area: 'sessions',
    route: `/sessions/${S.failed.id}`,
    title: 'Failed: harness out of memory',
    note: 'Crit banner with the humanised error; failed tool group; outcome with a failing check and uncertain items; continuation offers change approach.',
    api: page(S.failed, {
      events: [
        ...opening(S.failed.id, { branch: 'feat/build-cache', prompt: 'Keep target/ in a per-repository cache volume so a second session does not rebuild from scratch.' }),
        ev(S.failed.id, 'tool_call', { title: 'Bash: cargo build --release', kind: 'execute' }, -139500000, 'toolu_f1'),
        ev(S.failed.id, 'tool_result', { status: 'failed' }, -139100000, 'toolu_f1'),
        ev(S.failed.id, 'error', { harness_exit_code: 137, error: 'harness exited with code 137 (out of memory)' }, -139000000),
        ev(S.failed.id, 'state', { state: 'failed' }, -139000000),
      ],
      outcome: outcome(S.failed, {
        head_sha: '3e2d1c0b9a8f7e6d5c4b3a291807f6e5d4c3b2a1',
        verified: [{ check_id: 'chk-9', command: 'just test', outcome: 'failed', source_outcome: null, head_sha: '3e2d1c0b9a8f7e6d5c4b3a291807f6e5d4c3b2a1', passed: false, failed: true, current: true, finished_ms: -139200000 }],
        uncertain: ['The cache volume was created but the session ended before recording whether it was mounted read-write.'],
        needs_decision: [{ kind: 'report', id: 'rep-5', title: 'Build cache: what was tried', since_ms: -139000000 }],
      }),
      continuation: continuation(S.failed, { next: { kind: 'change_approach', text: 'The last attempt failed. Change the approach before trying again.', session_id: S.failed.id } }),
    }),
  },
  {
    id: 'sessions-killed-budget',
    area: 'sessions',
    route: `/sessions/${S.budget.id}`,
    title: 'Killed at budget',
    api: page(S.budget, {
      events: [...opening(S.budget.id, { branch: 'refactor/store-split' }), ev(S.budget.id, 'state', { state: 'killed_budget' }, -125000000)],
      detail: { usage: { ...usageReconciled, gateway_tokens: 2004112, harness_tokens: 1998000, charged_tokens: 2004112 } },
    }),
  },
  {
    id: 'sessions-cancelled',
    area: 'sessions',
    route: `/sessions/${S.killed.id}`,
    title: 'Stopped by the operator (cancelled)',
    note: 'No end banner for killed_user; outcome headline; continuation with Continue / Change approach / Abandon open on abandon.',
    api: page(S.killed, {
      events: [...opening(S.killed.id, { branch: 'spike/pty-capture' }), ev(S.killed.id, 'state', { state: 'closed' }, -118000000)],
      continuation: continuation(S.killed, { next: { kind: 'continue', text: 'You stopped this attempt. Continue it, or put it away.', session_id: S.killed.id } }),
    }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Abandon' }).click()
    },
  },

  // ---------------- panels and edges ----------------
  {
    id: 'sessions-authority-open',
    area: 'sessions',
    route: `/sessions/${S.waiting.id}#authority`,
    title: '"What this session may do" opened by its anchor',
    note: 'Scoped ask, a grant, refusals, unattended checks, the policy footer.',
    api: page(S.waiting, {
      events: [...opening(S.waiting.id), ev(S.waiting.id, 'permission_request', { title: 'Bash: cargo test -p tracon-node --test gateway' }, -95000, 'perm-41')],
      queue: { ...NO_QUEUE, waiting: [perm(S.waiting.id)] },
    }),
  },
  {
    id: 'sessions-authority-untrusted',
    area: 'sessions',
    route: `/sessions/${S.idle.id}#authority`,
    title: 'Authority with the policy bundle untrusted',
    note: 'Fail-closed warning; no egress, no credentials.',
    api: page(S.idle, {
      events: [...opening(S.idle.id)],
      authority: authority(S.idle, {
        policy: { version: 12, trusted: false, rules: 0 },
        actions: [
          { name: 'read', surface: 'tool', verdict: 'ask', rule_id: null, reason: null, scoped: [] },
          { name: 'edit', surface: 'tool', verdict: 'ask', rule_id: null, reason: null, scoped: [] },
          { name: 'execute', surface: 'tool', verdict: 'ask', rule_id: null, reason: null, scoped: [] },
        ],
        unattended_commands: [],
        access: { ...authority(S.idle).access, egress: [], credentials: [] },
      }),
    }),
  },
  {
    id: 'sessions-panels-error',
    area: 'sessions',
    route: `/sessions/${S.restart.id}#authority`,
    title: 'Authority, outcome and continuation all fail (500)',
    note: 'How each panel reports its own failure.',
    api: page(S.restart, {
      events: [...opening(S.restart.id, { branch: 'feat/awake-inhibitor' })],
      authority: fail(500, 'policy bundle could not be read: permission denied'),
      outcome: fail(500, 'database is locked'),
      continuation: fail(404, 'no such work item'),
    }),
  },
  {
    id: 'sessions-outcome-loading',
    area: 'sessions',
    route: `/sessions/${S.done.id}`,
    title: 'Outcome still being read',
    since: '#386',
    api: page(S.done, { events: [...opening(S.done.id)], outcome: never, continuation: continuation(S.done, { next: { kind: 'done', text: 'The work item is closed.', session_id: null }, actions: { continue_from: null, abandon: false } }) }),
  },
  {
    id: 'sessions-long',
    area: 'sessions',
    route: `/sessions/${S.long.id}`,
    title: 'Long everything: branch, paths, model, unbroken strings',
    note: 'Header wrapping, ceiling banner, repetition hint, unmetered usage, missing toolchain, orientation with omissions, an unbroken URL and hash in a message.',
    api: page(S.long, {
      events: [
        ev(S.long.id, 'session_started', { model: S.long.model, phase: 'execute', policy_version: 12, image_note: 'the repository image failed to build at 7c1e9a0 (apt-get: unable to locate package libssl1.1); falling back to the harness image' }, -2700000),
        ev(
          S.long.id,
          'orientation',
          {
            chars: 48212,
            text: 'You are executing work item wi-88 on channel client-acme-industries-long-channel.',
            missing: [
              { what: 'guide "Workspace" (`guide-workspace`)', partial: true, chars: 8000, fetch: 'call `doc_read` for `guide-workspace`' },
              { what: 'plan "Billing reconciliation rewrite, phase two: the ledger, the reconciler and the export" (`plan-billing-reconciliation-rewrite-phase-two`)', partial: false, chars: 21000, fetch: 'call `doc_read` for `plan-billing-reconciliation-rewrite-phase-two`' },
            ],
            context: { revision: 7, previous_revision: 6, changes: [{ kind: 'brief', slug: 'brief-billing', says: 'brief-billing changed: the export deadline moved to the end of the quarter' }], omitted: [] },
          },
          -2700000,
        ),
        ev(S.long.id, 'worktree', { path: S.long.worktree_path, branch: LONG_BRANCH, base: 'origin/release/2026.10-long-lived-maintenance-branch', main_checkout_dirty: true }, -2699000),
        ev(S.long.id, 'user_prompt', { text: 'Fix the reconciliation export. The failing run is https://ci.example.com/acme-industries/monorepo-with-a-long-name/-/pipelines/918273645/jobs/5647382910/artifacts/browse/target/reports/reconciliation-export-failure-2026-10-07T23-59-59Z.json' }, -2640000),
        ev(S.long.id, 'message', { text: 'The export fails on a row whose key is 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a089f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 — a sha256 doubled by a concatenation bug in ledger::key_for().' }, -2600000),
        ev(S.long.id, 'usage_unmetered', { turn: 2, gateway: { requests: 3 } }, -2500000),
        ev(S.long.id, 'ceiling', { usage_today: 2000000, ceiling: 2000000 }, -2400000),
        ev(S.long.id, 'gateway_refused', { provider: 'anthropic', method: 'POST', reason: 'channel at its daily ceiling' }, -2390000),
        ...Array.from({ length: 5 }, (_, i) => [
          ev(S.long.id, 'tool_call', { title: `Bash: cargo test -p billing-reconciliation --test export_roundtrip -- --nocapture --test-threads=1 ledger::key_for::handles_${i}`, kind: 'execute' }, -2300000 + i * 1000, `toolu_l${i}`),
          ev(S.long.id, 'tool_result', { status: i % 2 ? 'failed' : 'completed' }, -2299500 + i * 1000, `toolu_l${i}`),
        ]).flat(),
        ev(S.long.id, 'repetition', { what: 'tool_call', count: 5, title: 'Bash: cargo test -p billing-reconciliation --test export_roundtrip -- --nocapture --test-threads=1', kind: 'execute' }, -2200000),
      ],
      detail: {
        usage: { ...usageReconciled, state: 'unmetered', unmetered_turns: 2, gateway_tokens: 1876000, harness_tokens: 1870000, cache_read_tokens: 12400000 },
        ceiling: { usage_today: 2000000, ceiling: 2000000, state: 'at', unmetered_turns: 2 },
        toolchain: {
          revision: 'tc-7',
          image_revision: 'tc-5',
          tools: [
            { id: 'rust-analyzer', kind: 'lsp', version: '2026-09-29', path: '/opt/tools/rust-analyzer', state: 'unavailable' },
            { id: 'rustfmt', kind: 'formatter', version: '1.90.0', path: '/opt/tools/rustfmt', state: 'configured' },
          ],
        },
      },
    }),
  },
  {
    id: 'sessions-remote-unreachable',
    area: 'sessions',
    route: `/sessions/${S.remote.id}`,
    title: "A peer's session while the peer is unreachable",
    note: 'Node chip dims with last seen; Pause/Stop disabled; banner that the log resumes; placeholder says the prompt is queued.',
    api: page(S.remote, {
      events: [...opening(S.remote.id, { branch: 'feat/queue-metrics' })],
      api: {
        'GET /api/nodes': exact([
          { id: SELF, name: 'laptop', state: 'ready', failed_check: null, failed_detail: null, harness: { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false }, models: [{ value: 'sonnet', name: 'Sonnet' }], checked_at_ms: -420000, is_self: true, reachable: true, last_seen_ms: null, providers: [] },
          { id: PEER, name: 'work-pod', state: 'ready', failed_check: null, failed_detail: null, harness: { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false }, models: [{ value: 'sonnet', name: 'Sonnet' }], checked_at_ms: -2400000, is_self: false, reachable: false, last_seen_ms: -2400000, providers: [] },
        ]),
      },
    }),
  },
  {
    id: 'sessions-external',
    area: 'sessions',
    route: `/sessions/${S.external.id}`,
    title: 'An external agent attached on the channel',
    note: 'No repo, no budget: the header and banner say so; the controls fence broker access only.',
    api: page(S.external, {
      events: [
        ev(S.external.id, 'session_started', { model: 'external', phase: 'execute', policy_version: 12 }, -600000),
        ev(S.external.id, 'tool_call', { title: 'doc_search', kind: 'other' }, -500000, 'mcp_1'),
        ev(S.external.id, 'tool_result', { status: 'completed' }, -499000, 'mcp_1'),
        ev(S.external.id, 'tool_call', { title: 'submit_review', kind: 'other' }, -30000, 'mcp_2'),
      ],
    }),
  },
  {
    id: 'sessions-legacy',
    area: 'sessions',
    route: `/sessions/${S.legacy.id}`,
    title: 'A legacy session: readable, never launchable',
    api: page(S.legacy, {
      list: [S.legacy],
      events: [...opening(S.legacy.id, { branch: 'feat/early-gateway' })],
    }),
  },
  {
    id: 'sessions-not-found',
    area: 'sessions',
    route: '/sessions/00000000-dead-4bee-8000-000000000000',
    title: 'A session id the node does not know',
    note: 'Session absent from the list and the detail 404s.',
    api: {
      'GET /api/sessions': [S.idle],
      'GET /api/sessions/00000000-dead-4bee-8000-000000000000': fail(404, 'no such session'),
      'GET /api/sessions/00000000-dead-4bee-8000-000000000000/events': [],
      'GET /api/sessions/00000000-dead-4bee-8000-000000000000/draft': fail(404, 'no such session'),
      'GET /api/queue': NO_QUEUE,
    },
  },
  {
    id: 'sessions-phone-stop-confirm',
    area: 'sessions',
    route: `/sessions/${S.idle.id}`,
    title: 'Phone: Stop asks for a second tap',
    sizes: ['phone'],
    note: '"Stop — tap again" with Cancel beside it in the header.',
    api: page(S.idle, { events: [...opening(S.idle.id), ...workTurn(S.idle.id)] }),
    act: async (page) => {
      await page.locator('header.sess button', { hasText: 'Stop' }).click()
    },
  },

  // ---------------- OpenCode shell ----------------
  {
    id: 'sessions-opencode-shell',
    area: 'sessions',
    route: `/sessions/${S.working.id}/opencode`,
    title: 'OpenCode shell around the framed view',
    note: 'The frame content is a stand-in served from another origin; look at the slim bar: back, branch, state chip.',
    api: page(S.working, {
      api: {
        [`POST /api/sessions/${S.working.id}/opencode-boot`]: { url: 'http://localhost:5204/api/opencode-standin#/?boot=cap_9f2b7e1d4c', origin: 'http://localhost:5204', expires_ms: 60000, cookie_ttl_ms: 43200000 },
        'GET /api/opencode-standin': '[stand-in for OpenCode\'s own interface, served cross-origin by the node]',
      },
    }),
  },
  {
    id: 'sessions-opencode-shell-error',
    area: 'sessions',
    route: `/sessions/${S.working.id}/opencode`,
    title: 'OpenCode shell: boot refused',
    note: 'The node will not mint a capability: message and Try again.',
    api: page(S.working, {
      api: { [`POST /api/sessions/${S.working.id}/opencode-boot`]: fail(409, "this session's OpenCode server is not listening yet; it starts with the harness") },
    }),
  },
  {
    id: 'sessions-opencode-shell-bad-url',
    area: 'sessions',
    route: `/sessions/${S.working.id}/opencode`,
    title: 'OpenCode shell: boot URL refused by the page',
    note: 'A boot URL on this page\'s own origin is refused before framing.',
    api: page(S.working, {
      api: { [`POST /api/sessions/${S.working.id}/opencode-boot`]: { url: 'http://127.0.0.1:5204/#/?boot=cap_9f2b7e1d4c', origin: 'http://127.0.0.1:5204', expires_ms: 60000 } },
    }),
  },
  {
    id: 'sessions-opencode-shell-loading',
    area: 'sessions',
    route: `/sessions/${S.working.id}/opencode`,
    title: 'OpenCode shell: opening',
    api: page(S.working, { api: { [`POST /api/sessions/${S.working.id}/opencode-boot`]: never } }),
  },
]
