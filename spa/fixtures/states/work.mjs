// The work ledger (/work), one work item (/work/:id) with its continuation,
// brief, criteria, context and evidence panels, and the composer a work item
// sends a phase to (/?item=…&phase=…) with its readiness line and preparation
// preview.
//
// Every item state overrides /api/work (the item page reads it for dependency
// titles), /api/work/:id, its continuation and context, and the evidence list,
// so no state leans on the baseline's single item.

import { readFileSync } from 'node:fs'

const baseline = JSON.parse(readFileSync(new URL('../api.json', import.meta.url), 'utf8'))

const M = 60_000
const H = 3_600_000
const D = 86_400_000
const SELF = baseline['/api/node'].id

// Ids as the node mints them: hex, long enough that `short()` matters.
const ID = {
  limits: 'b07d13e8a2f94c61',
  retry: '4e2a9c0d71b85f36',
  export: 'c81f5e2b09a4d7e3',
  alert: '9d3b6a1f4c0e8275',
  cookies: '1a6e0f3c8b9d2475',
  metrics: 'e5c72d4190fb3a68',
  docsync: '72f0b9e4a13c6d58',
  mesh: 'f3a8d1c6e72b0954',
  audit: '0c4f9e7a2d61b835',
  long: 'a9e1c4f7b2d06358',
}
const SID = {
  plan: '3f9a2c71e0b4d865',
  exec1: '8b4e0d2a6c19f753',
  exec2: 'd27c5f8e1a3b6094',
  run: '6a0f3e9c2b7d1458',
  review: '5c8d2a0f7e4b1963',
  other: '0e7b4c2d9f1a8536',
}

const item = (o) => ({
  id: ID.limits,
  channel: 'personal',
  project_id: 'orbit',
  title: 'Add per-channel rate limits',
  body: '',
  state: 'open',
  priority: 0,
  deps: [],
  discovered_from: null,
  discovered_by_session: null,
  phase_plan_slug: null,
  brief_slug: null,
  closed_by_session: null,
  created_ms: -2 * D,
  updated_ms: -H,
  readiness: { state: 'ready' },
  session_id: null,
  ...o,
})

const session = (o) => ({
  ...baseline['/api/sessions/*'].session,
  node_id: SELF,
  channel: 'personal',
  work_item_id: ID.limits,
  repo_path: '/home/op/src/orbit',
  worktree_path: null,
  branch: 'feat/rate-limits',
  harness_agent: null,
  harness_found: '2.5.0',
  harness_protocol: null,
  review_id: null,
  draft: null,
  state: 'closed',
  end_reason: 'phase_done',
  last_error: null,
  turn_active: 0,
  parent_session: null,
  continued_from: null,
  ...o,
})

const attempt = (o) => ({
  id: SID.plan,
  phase: 'plan',
  model: 'sonnet',
  harness: 'claude',
  state: 'closed',
  end_reason: 'phase_done',
  last_error: null,
  tokens_used: 186_000,
  created_ms: -26 * H,
  continued_from: null,
  parent_session: null,
  archived: false,
  ...o,
})

const continuation = (it, o) => ({
  kind: 'item',
  id: it.id,
  channel: it.channel,
  intent: { title: it.title, body: it.body, source: 'item' },
  attempts: [],
  blockers: [],
  next: { kind: 'start', text: 'Nothing has run yet: plan it.', session_id: null },
  workspace: null,
  decisions: { plan: it.phase_plan_slug, brief: it.brief_slug, answered: [] },
  evidence: { reviews: [], shown: [] },
  actions: { continue_from: null, abandon: it.state === 'open' },
  ...o,
})

const noContext = { slug: '', selection: null, attempts: [], roles: ['brief', 'research', 'decisions', 'constraints', 'documents'] }
const noEvidence = { items: [], next_before: null }

// The node's order: open items by priority desc then age, closed by last update.
const order = (items) => [
  ...items.filter((i) => i.state === 'open').sort((a, b) => b.priority - a.priority || a.created_ms - b.created_ms),
  ...items.filter((i) => i.state !== 'open').sort((a, b) => b.updated_ms - a.updated_ms),
]

// The ledger every item page reads for titles, and the list screen's default.
const ledger = order([
  item({ id: ID.limits, title: 'Add per-channel rate limits', priority: 2, phase_plan_slug: 'plan-rate-limits', created_ms: -2 * D }),
  item({ id: ID.retry, title: 'Retry queue for failed pushes', priority: 1, phase_plan_slug: 'plan-retry-queue', created_ms: -3 * D, session_id: SID.run, updated_ms: -40 * M }),
  item({ id: ID.export, title: 'Nightly export of the docs corpus to object storage', discovered_from: ID.limits, discovered_by_session: SID.exec1, created_ms: -35 * M }),
  item({ id: ID.alert, title: 'Alert when the retry queue stalls', deps: [ID.retry], readiness: { state: 'blocked', by: [{ kind: 'open', id: ID.retry }] }, created_ms: -2 * D }),
  item({ id: ID.mesh, title: 'Mirror review verdicts to the hub', deps: ['5b1e7c3f0a9d2486'], readiness: { state: 'blocked', by: [{ kind: 'unknown', id: '5b1e7c3f0a9d2486' }] }, created_ms: -6 * D }),
  item({ id: ID.metrics, title: 'Metrics rollups per channel', priority: 5, phase_plan_slug: 'plan-metrics-rollups', created_ms: -9 * H }),
  item({ id: ID.cookies, title: 'Session cookies over remember-me tokens', state: 'closed', priority: 1, phase_plan_slug: 'plan-session-cookies', closed_by_session: SID.exec2, readiness: { state: 'closed' }, created_ms: -4 * D, updated_ms: -25 * H }),
  item({ id: ID.docsync, title: 'Sync document edits between peers', state: 'closed', readiness: { state: 'closed' }, created_ms: -12 * D, updated_ms: -8 * D }),
])
const titleOf = Object.fromEntries(ledger.map((i) => [i.id, i]))

const runningSession = session({ id: SID.run, work_item_id: ID.retry, phase: 'execute', state: 'running', end_reason: null, branch: 'feat/retry-queue', tokens_used: 238_000, created_ms: -40 * M, updated_ms: -M })
const sessionsWithRun = [...baseline['/api/sessions'], runningSession]

/** Everything an item page asks for, built around one item. */
function itemApi(it, o = {}) {
  const path = `/api/work/${it.id}`
  const others = ledger.filter((l) => l.id !== it.id)
  return {
    '/api/work': { items: [it, ...others] },
    [path]: {
      item: it,
      sessions: o.sessions ?? [],
      discovered: o.discovered ?? [],
      brief: o.brief ?? null,
      criteria: o.criteria ?? null,
    },
    [`${path}/continuation`]: o.continuation ?? (it === planned ? plannedContinuation : continuation(it)),
    [`${path}/context`]: o.context ?? noContext,
    '/api/evidence/candidates': o.evidence ?? noEvidence,
    '/api/sessions': sessionsWithRun,
    ...(o.api ?? {}),
  }
}

// A brief with every kind of line, and a section left empty.
const brief = (it, o = {}) => ({
  channel: it.channel,
  slug: `brief-${it.id.slice(0, 8)}`,
  work_item_id: it.id,
  hash: 'b4f1',
  updated_ms: -5 * H,
  title: `Brief: ${it.title}`,
  preamble: 'Written after the gateway incident on the shared node; refine before the execute session.',
  sections: [
    {
      field: 'intended_user',
      heading: 'Intended user',
      notes: '',
      entries: [
        { provenance: 'observed', text: 'An operator running two channels from one laptop, one of them for a client.', refs: [{ kind: 'doc', value: 'meeting-ops-sync', label: 'Ops sync, 3 Oct', known: true }] },
      ],
    },
    {
      field: 'problem',
      heading: 'Problem',
      notes: '',
      entries: [
        { provenance: 'observed', text: 'A burst of tool calls from one session starved the other channel for four minutes.', refs: [{ kind: 'doc', value: 'ref-burst-traces', label: 'Burst traces', known: true }, { kind: 'session', value: SID.exec1 }] },
        { provenance: 'inferred', text: 'The gateway ceiling is daily, so nothing smooths a burst inside the day.', refs: [{ kind: 'file', value: 'node/src/gateway/ceiling.rs' }] },
      ],
    },
    { field: 'source_references', heading: 'Source references', notes: '', entries: [{ provenance: 'unattributed', text: 'Token bucket notes from the earlier design.', refs: [{ kind: 'doc', value: 'note-token-bucket-old', known: false }] }] },
    {
      field: 'constraints',
      heading: 'Constraints',
      notes: '',
      entries: [{ provenance: 'decided', text: 'No new table: the limiter keeps its state in memory and resets on restart.', refs: [] }],
    },
    {
      field: 'success_criteria',
      heading: 'Success criteria',
      notes: '',
      entries: [
        { provenance: 'decided', text: 'A burst on one channel never delays another channel by more than a second.', refs: [] },
        { provenance: 'inferred', text: 'The limit is configurable per channel in node.toml.', refs: [] },
      ],
    },
  ],
  extra: '',
  counts: { observed: 2, inferred: 2, decided: 2, unattributed: 1 },
  absent: [{ field: 'unresolved_questions', heading: 'Unresolved questions', says: 'no open questions recorded' }],
  ...o,
})

const criterion = (o) => ({
  key: 'c1',
  text: 'A burst on one channel never delays another channel by more than a second.',
  provenance: 'decided',
  standard: 'agreed',
  refs: [],
  links: [],
  coverage: 'nothing_points_at_it',
  ...o,
})

const criteria = (it, o = {}) => ({
  work_item_id: it.id,
  channel: it.channel,
  brief_slug: `brief-${it.id.slice(0, 8)}`,
  hash: 'b4f1',
  candidate: { id: 'cand-7e21', head_sha: 'e4b19c07a2d35f86e4b19c07a2d35f86e4b19c07', captured_ms: -50 * M },
  criteria: [
    criterion({
      key: 'c1',
      links: [{ index: 0, provenance: 'decided', standard: 'agreed', kind: 'check', value: 'cargo nextest run -p tracon-node gateway::limits', refs: [], outcome: 'passed', run_id: 'run-41' }],
      coverage: 'checks_pass',
    }),
    criterion({
      key: 'c2',
      text: 'The limit is configurable per channel in node.toml.',
      provenance: 'inferred',
      standard: 'proposed',
      links: [{ index: 0, provenance: 'inferred', standard: 'proposed', kind: 'check', value: 'cargo test config::channel_limits', refs: [], unresolved: 'not one of the checks the operator configured, so it never runs' }],
      coverage: 'only_proposed',
    }),
    criterion({
      key: 'c3',
      text: 'Nothing new is written to disk by the limiter.',
      links: [{ index: 0, provenance: 'decided', standard: 'agreed', kind: 'check', value: 'just test', refs: [], outcome: 'failed', run_id: 'run-42' }],
      coverage: 'failing',
    }),
    criterion({
      key: 'c4',
      text: 'Operators can see why a call was delayed.',
      links: [],
      coverage: 'judged_met',
      judgement: { id: 'j-1', verdict: 'met', note: 'the session log names the limiter and the wait', candidate_id: 'cand-7e21', criterion_text: 'Operators can see why a call was delayed.', judged_ms: -30 * M },
    }),
  ],
  gaps: {
    uncovered: ['c2'],
    unjudged: ['c1', 'c2', 'c3'],
    assumptions: [{ field: 'problem', heading: 'Problem', text: 'The gateway ceiling is daily, so nothing smooths a burst inside the day.', provenance: 'inferred' }],
    questions: [],
    orphaned_judgements: [],
  },
  summary: '4 criteria · 1 only the agent\'s proposal · 1 failing · 1 checks pass, unjudged · 1 judged met · 1 not an agreed standard · 1 assumptions',
  ...o,
})

const contextFull = (it) => ({
  slug: `context-${it.id.slice(0, 8)}`,
  roles: ['brief', 'research', 'decisions', 'constraints', 'documents'],
  selection: {
    channel: it.channel,
    slug: `context-${it.id.slice(0, 8)}`,
    work_item_id: it.id,
    hash: 'c2',
    updated_ms: -3 * H,
    title: `Context: ${it.title}`,
    preamble: '',
    picks: [
      { role: 'brief', slug: `brief-${it.id.slice(0, 8)}`, note: '' },
      { role: 'research', slug: 'ref-burst-traces', note: "three bursts from last week's traces" },
      { role: 'decisions', slug: 'note-token-bucket', note: '' },
      { role: 'constraints', slug: 'guide-gateway-latency', note: 'p99 budget' },
      { role: 'documents', slug: 'ref-gateway-dashboard', note: '' },
    ],
    extra: '',
    resolved: [
      { role: 'brief', slug: `brief-${it.id.slice(0, 8)}`, title: `Brief: ${it.title}`, known: true, chars: 2100 },
      { role: 'research', slug: 'ref-burst-traces', title: 'Burst traces', known: true, chars: 4200 },
      { role: 'decisions', slug: 'note-token-bucket', title: 'Token bucket, not leaky', known: true, chars: 1800 },
      { role: 'constraints', slug: 'guide-gateway-latency', known: false, chars: 0, reason: 'absent' },
      { role: 'documents', slug: 'ref-gateway-dashboard', title: 'Gateway dashboard', known: true, chars: 9000, reason: 'html' },
    ],
  },
  attempts: [
    {
      session_id: SID.exec1,
      work_item_id: it.id,
      channel: it.channel,
      revision: 2,
      digest: 'd2',
      selection_hash: 'c2',
      received: [
        { role: 'brief', slug: `brief-${it.id.slice(0, 8)}`, title: `Brief: ${it.title}`, hash: 'hb', chars: 2100, delivered_chars: 2100, delivery: 'full' },
        { role: 'research', slug: 'ref-burst-traces', title: 'Burst traces', hash: 'h1b', chars: 42000, delivered_chars: 24000, delivery: 'partial', reason: 'cap' },
        { role: 'decisions', slug: 'note-token-bucket', title: 'Token bucket, not leaky', hash: 'h2', chars: 1800, delivered_chars: 1800, delivery: 'full' },
        { role: 'constraints', slug: 'guide-gateway-latency', chars: 0, delivered_chars: 0, delivery: 'omitted', reason: 'absent' },
      ],
      previous_session: SID.plan,
      previous_revision: 1,
      changes: [
        { kind: 'edited', slug: 'ref-burst-traces', role: 'research', says: '`ref-burst-traces` edited since the previous attempt' },
        { kind: 'added', slug: 'guide-gateway-latency', role: 'constraints', says: '`guide-gateway-latency` added as constraints' },
        { kind: 'delivery', slug: 'ref-burst-traces', role: 'research', says: '`ref-burst-traces` was delivered in full, now cut short' },
      ],
      created_ms: -6 * H,
    },
    {
      session_id: SID.plan,
      work_item_id: it.id,
      channel: it.channel,
      revision: 1,
      digest: 'd1',
      selection_hash: 'c1',
      received: [
        { role: 'research', slug: 'ref-burst-traces', title: 'Burst traces', hash: 'h1a', chars: 3900, delivered_chars: 3900, delivery: 'full' },
        { role: 'decisions', slug: 'note-token-bucket', title: 'Token bucket, not leaky', hash: 'h2', chars: 1800, delivered_chars: 1800, delivery: 'full' },
      ],
      changes: [],
      created_ms: -26 * H,
    },
  ],
})

const evidence = (it, n = 2) => ({
  items: Array.from({ length: n }, (_, i) => ({
    candidate: {
      id: `cand-${i}`,
      head_sha: ['e4b19c07a2d35f86e4b1', '7a0c2e9f41d8b36c5e2a', '19d4f7a3c0e2b85d6f41'][i % 3],
      channel: it.channel,
      owner_session_id: SID.exec2,
      owner_node_id: null,
      source_kind: 'review',
      captured_ms: -(50 + i * 180) * M,
    },
    work_items: [{ id: it.id, title: it.title, state: it.state }],
    more_work_items: false,
    reviews: [{ id: `rv-${i}`, title: `${i ? 'wip: ' : ''}${it.title.toLowerCase()}`, state: i ? 'changes_requested' : it.state === 'closed' ? 'approved' : 'pending' }],
    more_reviews: false,
  })),
  next_before: null,
})

// The evidence list is asked once per session on an item page (or once for the
// item when it has none); only the sessions named here captured anything.
const evidenceBy = (bySession) => (req) => bySession[req.query.session_id] ?? noEvidence

// The planned, worked-on item most panels are shown on.
const planned = item({
  id: ID.limits,
  title: 'Add per-channel rate limits',
  body: 'The gateway refuses over-ceiling calls but nothing smooths bursts.\n\nAdd a token bucket per channel in front of the broker. Keep it in memory; no new table.',
  priority: 2,
  phase_plan_slug: 'plan-rate-limits',
  brief_slug: `brief-${ID.limits.slice(0, 8)}`,
  created_ms: -2 * D,
})
const planSession = session({ id: SID.plan, phase: 'plan', end_reason: 'phase_done', tokens_used: 186_000, budget_tokens: 400_000, created_ms: -26 * H })
const planAttempt = attempt({ id: SID.plan })
const plannedContinuation = continuation(planned, {
  attempts: [planAttempt],
  next: { kind: 'execute', text: 'The plan is written: execute it.', session_id: SID.plan },
  workspace: { id: SID.plan.slice(0, 12), branch: 'feat/rate-limits', session_id: SID.plan },
  actions: { continue_from: SID.plan, abandon: true },
})

// The composer reached from an item. Its readiness line and preparation
// preview answer for the repository the channel used last.
const readinessAll = {
  channel: 'personal',
  repo: '/home/op/src/orbit',
  investigate: { purpose: 'investigate', ready: true, missing: [], notes: [] },
  verify: { purpose: 'verify', ready: true, missing: [], notes: [] },
  publish: { purpose: 'publish', ready: true, missing: [], notes: [] },
}
const prepClean = {
  repo: '/home/op/src/orbit',
  image: 'ghcr.io/op/orbit-toolchain@sha256:4f1c9e0a7b2d',
  image_source: 'repository toolchain',
  devcontainer_image: null,
  lockfiles: ['Cargo.lock', 'bun.lock'],
  install: 'cargo fetch --locked',
  prepare: ['just spa-build'],
  egress: ['index.crates.io', 'registry.npmjs.org'],
  incompatible: [],
  ready: true,
}
const launchApi = (it, o = {}) => ({
  [`/api/work/${it.id}`]: { item: it, sessions: [], discovered: [], brief: null, criteria: null },
  '/api/readiness': o.readiness ?? readinessAll,
  '/api/preparation': o.preparation ?? prepClean,
  ...(o.api ?? {}),
})
const openAdjust = async (page) => {
  await page.getByRole('button', { name: 'adjust' }).click()
  await page.waitForTimeout(500)
}
const openDetails = (selector) => async (page) => {
  await page.$$eval(selector, (els) => els.forEach((e) => (e.open = true)))
}

const err = (status, message) => ({ status, body: { error: { code: status, message } } })
const never = () => new Promise(() => {})

const longTitle =
  'Rework the publication path so a review approved on one node publishes from the node that holds the forge credential, with the approved message and branch, and a retry that never double-pushes'
const longUnbroken = 'feat/rework-publication-path-so-approved-reviews-publish-from-the-credential-holding-node-without-double-pushing'

const states = [
  // ---------------------------------------------------------------- ledger
  {
    id: 'work-list',
    area: 'work',
    route: '/work',
    title: 'Ledger: ready, blocked, in session, closed',
    note: 'every row state at once: ready with and without a plan, blocked by an open item and by an unknown one, in session with its holder, p5 priority colour, closed count',
    api: { '/api/work': { items: ledger }, '/api/sessions': sessionsWithRun },
  },
  {
    id: 'work-list-closed-shown',
    area: 'work',
    route: '/work',
    title: 'Ledger with closed items shown',
    api: { '/api/work': { items: ledger }, '/api/sessions': sessionsWithRun },
    act: async (page) => page.getByRole('button', { name: 'Show' }).click(),
  },
  {
    id: 'work-empty',
    area: 'work',
    route: '/work',
    title: 'No work on the channel',
    api: { '/api/work': { items: [] } },
  },
  {
    id: 'work-only-closed',
    area: 'work',
    route: '/work',
    title: 'Every item closed',
    note: 'header says 0 open · 2 closed; the empty line reads as if nothing was ever added',
    api: { '/api/work': { items: ledger.filter((i) => i.state === 'closed') } },
  },
  {
    id: 'work-all-blocked',
    area: 'work',
    route: '/work',
    title: 'Every open item blocked',
    api: {
      '/api/work': {
        items: [
          ledger.find((i) => i.id === ID.alert),
          ledger.find((i) => i.id === ID.mesh),
          item({ id: ID.audit, title: 'Audit log export', deps: [ID.metrics, ID.audit], readiness: { state: 'blocked', by: [{ kind: 'cycle' }] } }),
        ],
      },
    },
  },
  {
    id: 'work-all-in-session',
    area: 'work',
    route: '/work',
    title: 'Every open item already in a session',
    api: { '/api/work': { items: [ledger.find((i) => i.id === ID.retry)] }, '/api/sessions': sessionsWithRun },
  },
  {
    id: 'work-filtered-empty',
    area: 'work',
    route: '/work',
    title: 'Switched to a channel with no work',
    note: 'channel select on "work", which has nothing',
    api: { '/api/work': (req) => ({ items: req.query.channel === 'work' ? [] : ledger }) },
    act: async (page) => page.selectOption('.bar select', 'work'),
  },
  {
    id: 'work-many',
    area: 'work',
    route: '/work',
    title: 'Forty items, long titles',
    note: 'ellipsis on titles and the detail line; priority column width at p10+',
    api: {
      '/api/work': {
        items: order(Array.from({ length: 40 }, (_, i) => {
          const blocked = i % 7 === 3
          const depId = `${(i + 1).toString(16).padStart(4, '0')}a7c9e2b4d1f6`
          return item({
            id: `${i.toString(16).padStart(4, '0')}f3a8d1c6e72b`,
            title: i % 5 === 0 ? `${longTitle} (${i})` : ['Retry queue for failed pushes', 'Metrics rollups per channel', 'Mirror review verdicts to the hub', 'Keep the verdict bar in reach on long diffs'][i % 4],
            priority: i < 3 ? 12 - i : i % 4,
            phase_plan_slug: i % 2 ? `plan-item-${i}` : null,
            deps: blocked ? [depId] : [],
            readiness: blocked ? { state: 'blocked', by: [{ kind: 'unknown', id: depId }] } : { state: 'ready' },
            discovered_from: i % 6 === 1 ? ID.limits : null,
            created_ms: -(i + 1) * 5 * H,
          })
        })),
      },
    },
  },
  {
    id: 'work-long-unbroken',
    area: 'work',
    route: '/work',
    title: 'Unbroken title strings',
    note: 'a title that is one long token; ellipsis must hold on phone',
    api: {
      '/api/work': {
        items: [
          item({ id: ID.long, title: longUnbroken, priority: 3 }),
          item({ id: ID.export, title: `docs-sync:${'x'.repeat(10)}::${'/var/lib/tracon/repos/github.com/op/infra/kubernetes/apps/tracon/hub/values.yaml'}` }),
        ],
      },
    },
  },
  {
    id: 'work-loading',
    area: 'work',
    route: '/work',
    title: 'Ledger still loading',
    note: 'the list request held open: is anything said, or does it claim the channel is empty?',
    api: { '/api/work': never },
  },
  {
    id: 'work-load-error',
    area: 'work',
    route: '/work',
    title: 'Ledger failed to load',
    api: { '/api/work': err(500, 'database is locked') },
  },
  {
    id: 'work-new-form',
    area: 'work',
    route: '/work',
    title: 'New work item form',
    api: { '/api/work': { items: ledger }, '/api/sessions': sessionsWithRun },
    act: async (page) => {
      await page.getByRole('button', { name: 'New work item' }).click()
      await page.fill('.new input', 'Cap the retry queue at the same window')
      await page.fill('.new textarea', 'Same token bucket as the gateway. Done when a stalled peer never holds more than 500 pushes.')
      await page.fill('.new .pri', '3')
    },
  },
  {
    id: 'work-new-form-refused',
    area: 'work',
    route: '/work',
    title: 'New work item refused',
    note: 'an add that fails shows its error under the heading "Could not load work"',
    api: {
      '/api/work': { items: ledger },
      'POST /api/work': err(409, 'channel personal is archived; it keeps its work and takes no new items'),
      '/api/sessions': sessionsWithRun,
    },
    act: async (page) => {
      await page.getByRole('button', { name: 'New work item' }).click()
      await page.fill('.new input', 'Cap the retry queue at the same window')
      await page.getByRole('button', { name: 'Add work item' }).click()
    },
  },

  // ------------------------------------------------------------- an item
  {
    id: 'work-item-new',
    area: 'work',
    route: `/work/${ID.export}`,
    title: 'New item: no plan, nothing run, no brief or context',
    since: '#385',
    note: 'continuation says plan it, and nothing else, since Execute is disabled with "needs a plan"; empty brief, criteria, context and evidence; discovered-from line',
    api: itemApi(ledger.find((i) => i.id === ID.export), {
      continuation: continuation(ledger.find((i) => i.id === ID.export), { decisions: { plan: null, brief: null, answered: [] } }),
    }),
  },
  {
    id: 'work-item-planned',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Planned item with brief, criteria, context, evidence',
    since: '#385',
    note: 'continuation "The plan is written: execute it." — the node offers continue_from the plan session, so the primary button is Continue, which the node carries on as an execute session in the plan\'s workspace. Brief, criteria, context populated; evidence empty (only a plan ran)',
    api: itemApi(planned, {
      sessions: [planSession],
      brief: brief(planned),
      criteria: criteria(planned),
      context: contextFull(planned),
      continuation: continuation(planned, {
        attempts: [planAttempt],
        next: { kind: 'execute', text: 'The plan is written: execute it.', session_id: SID.plan },
        workspace: { id: SID.plan.slice(0, 12), branch: 'feat/rate-limits', session_id: SID.plan },
        actions: { continue_from: SID.plan, abandon: true },
      }),
    }),
  },
  {
    id: 'work-item-in-session',
    area: 'work',
    route: `/work/${ID.retry}`,
    title: 'In session: a session is working on it',
    since: '#385',
    note: 'continuation "watch" with an Open link; Open session primary action',
    api: (() => {
      const it = ledger.find((i) => i.id === ID.retry)
      return itemApi(it, {
        sessions: [session({ id: SID.plan, work_item_id: it.id, phase: 'plan', created_ms: -3 * D }), runningSession],
        continuation: continuation(it, {
          attempts: [attempt({ id: SID.plan, created_ms: -3 * D }), attempt({ id: SID.run, phase: 'execute', model: 'opus', state: 'running', end_reason: null, tokens_used: 238_000, created_ms: -40 * M })],
          next: { kind: 'watch', text: `Session ${SID.run.slice(0, 8)} is working on it.`, session_id: SID.run },
          workspace: { id: SID.run.slice(0, 12), branch: 'feat/retry-queue', session_id: SID.run },
        }),
      })
    })(),
  },
  {
    id: 'work-item-waiting-on-you',
    area: 'work',
    route: `/work/${ID.retry}`,
    title: 'In session, waiting on the operator',
    since: '#385',
    note: 'next kind "answer" (wait-coloured edge), blockers line, decisions already answered',
    api: (() => {
      const it = ledger.find((i) => i.id === ID.retry)
      const s = session({ ...runningSession, state: 'waiting_on_you' })
      return itemApi(it, {
        sessions: [s],
        continuation: continuation(it, {
          attempts: [attempt({ id: SID.run, phase: 'execute', model: 'opus', state: 'waiting_on_you', end_reason: null, tokens_used: 238_000, created_ms: -40 * M })],
          blockers: [`session ${SID.run.slice(0, 8)} waits on 2 answers`],
          next: { kind: 'answer', text: `Session ${SID.run.slice(0, 8)} is waiting on you: 2 requests to answer.`, session_id: SID.run },
          workspace: { id: SID.run.slice(0, 12), branch: 'feat/retry-queue', session_id: SID.run },
          decisions: {
            plan: 'plan-retry-queue',
            brief: null,
            answered: [
              { session_id: SID.run, kind: 'permission', asked: 'Bash: cargo nextest run -p tracon-node', answer: 'allow_always', at_ms: -30 * M },
              { session_id: SID.run, kind: 'question', asked: 'Should a push that fails while offline be retried after the device comes back, or dropped after an hour?', answer: 'retry with backoff, give up after 24h', at_ms: -12 * M },
            ],
          },
        }),
      })
    })(),
  },
  {
    id: 'work-item-paused',
    area: 'work',
    route: `/work/${ID.retry}`,
    title: 'In session, paused by the node',
    since: '#384',
    note: 'next "resume" with the reason the node paused it',
    api: (() => {
      const it = ledger.find((i) => i.id === ID.retry)
      const s = session({ ...runningSession, state: 'paused', last_error: 'anthropic: usage limit reached until 14:00.' })
      return itemApi(it, {
        sessions: [s],
        continuation: continuation(it, {
          attempts: [attempt({ id: SID.run, phase: 'execute', model: 'opus', state: 'paused', end_reason: null, tokens_used: 238_000, created_ms: -40 * M })],
          blockers: [`session ${SID.run.slice(0, 8)} is paused`],
          next: { kind: 'resume', text: `Session ${SID.run.slice(0, 8)} is paused: anthropic: usage limit reached until 14:00. Resume it or stop it.`, session_id: SID.run },
          workspace: { id: SID.run.slice(0, 12), branch: 'feat/retry-queue', session_id: SID.run },
        }),
      })
    })(),
  },
  {
    id: 'work-item-blocked',
    area: 'work',
    route: `/work/${ID.alert}`,
    title: 'Blocked: waits on an open item, an unknown one, and a cycle',
    since: '#385',
    note: 'Waits on list with × removers; continuation "unblock"; Plan/Execute not offered',
    api: (() => {
      const it = item({
        id: ID.alert,
        title: 'Alert when the retry queue stalls',
        deps: [ID.retry, '5b1e7c3f0a9d2486', ID.audit],
        readiness: { state: 'blocked', by: [{ kind: 'open', id: ID.retry }, { kind: 'unknown', id: '5b1e7c3f0a9d2486' }, { kind: 'cycle' }] },
      })
      return itemApi(it, {
        continuation: continuation(it, {
          next: { kind: 'unblock', text: `Waits on “Retry queue for failed pushes” (${ID.retry.slice(0, 8)}), item 5b1e7c3f not seen on this node, a dependency cycle.`, session_id: null },
        }),
      })
    })(),
  },
  {
    id: 'work-item-node-restart',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Cut off by a node restart: continue offered',
    since: '#375',
    note: 'Continue primary, Change approach secondary, Abandon; attempt reads "node restarted"',
    api: itemApi(planned, {
      sessions: [planSession, session({ id: SID.exec1, phase: 'execute', model: 'opus', state: 'closed', end_reason: 'node_restart', tokens_used: 512_000, budget_tokens: 2_000_000, created_ms: -6 * H })],
      brief: brief(planned),
      criteria: criteria(planned),
      context: contextFull(planned),
      continuation: continuation(planned, {
        attempts: [planAttempt, attempt({ id: SID.exec1, phase: 'execute', model: 'opus', end_reason: 'node_restart', tokens_used: 512_000, created_ms: -6 * H })],
        next: { kind: 'continue', text: `Session ${SID.exec1.slice(0, 8)} was cut off by a node restart; continue from its workspace.`, session_id: SID.exec1 },
        workspace: { id: SID.exec1.slice(0, 12), branch: 'feat/rate-limits', session_id: SID.exec1 },
        actions: { continue_from: SID.exec1, abandon: true },
      }),
    }),
  },
  {
    id: 'work-item-failed',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Attempt failed: change approach comes first',
    since: '#385',
    note: 'next "change_approach" edge, Change approach primary, no plain Continue; continued lineage "continues 8b4e0d2a"; reviews in evidence',
    api: itemApi(planned, {
      sessions: [
        planSession,
        session({ id: SID.exec1, phase: 'execute', model: 'opus', end_reason: 'node_restart', tokens_used: 512_000, budget_tokens: 2_000_000, created_ms: -6 * H }),
        session({ id: SID.exec2, phase: 'execute', model: 'opus', state: 'failed', end_reason: 'error', last_error: 'tests never passed', tokens_used: 1_204_000, budget_tokens: 2_000_000, continued_from: SID.exec1, created_ms: -3 * H }),
      ],
      brief: brief(planned),
      criteria: criteria(planned),
      context: contextFull(planned),
      evidence: evidenceBy({ [SID.exec2]: evidence(planned, 3) }),
      continuation: continuation(planned, {
        attempts: [
          planAttempt,
          attempt({ id: SID.exec1, phase: 'execute', model: 'opus', end_reason: 'node_restart', tokens_used: 512_000, created_ms: -6 * H }),
          attempt({ id: SID.exec2, phase: 'execute', model: 'opus', state: 'failed', end_reason: 'error', last_error: 'tests never passed', tokens_used: 1_204_000, continued_from: SID.exec1, created_ms: -3 * H }),
        ],
        next: { kind: 'change_approach', text: `Session ${SID.exec2.slice(0, 8)} failed: tests never passed. Continue with a different approach, or abandon it.`, session_id: SID.exec2 },
        workspace: { id: SID.exec2.slice(0, 12), branch: 'feat/rate-limits', session_id: SID.exec2 },
        evidence: {
          reviews: [
            { id: 'rv-0', session_id: SID.exec2, title: 'feat(gateway): per-channel token buckets', state: 'changes_requested', verdict_reason: 'the limiter writes to disk', publish_result: null, head_sha: 'e4b19c07a2d3', created_ms: -2 * H },
          ],
          shown: [{ id: 'sw-1', session_id: SID.exec2, title: 'Limiter under a 200-call burst', head_sha: '7a0c2e9f41d8', stale: true, created_ms: -150 * M }],
        },
        actions: { continue_from: SID.exec2, abandon: true },
      }),
    }),
  },
  {
    id: 'work-item-change-approach-form',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Change approach: direction typed',
    since: '#385',
    api: itemApi(planned, {
      sessions: [planSession, session({ id: SID.exec2, phase: 'execute', model: 'opus', state: 'killed_budget', end_reason: 'budget', tokens_used: 2_000_000, budget_tokens: 2_000_000, created_ms: -3 * H })], brief: brief(planned), criteria: criteria(planned),
      continuation: continuation(planned, {
        attempts: [planAttempt, attempt({ id: SID.exec2, phase: 'execute', model: 'opus', state: 'killed_budget', end_reason: 'budget', tokens_used: 2_000_000, created_ms: -3 * H })],
        next: { kind: 'change_approach', text: `Session ${SID.exec2.slice(0, 8)} spent its budget. Continue with a different approach, or abandon it.`, session_id: SID.exec2 },
        workspace: { id: SID.exec2.slice(0, 12), branch: 'feat/rate-limits', session_id: SID.exec2 },
        actions: { continue_from: SID.exec2, abandon: true },
      }),
    }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Change approach' }).click()
      await page.fill('textarea[aria-label="New approach"]', 'Stop rewriting the broker. Put the bucket in the gateway in front of it and leave the broker untouched; run only the gateway tests until they pass.')
    },
  },
  {
    id: 'work-item-abandon-confirm',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Abandon: the confirm step',
    since: '#385',
    api: itemApi(planned, {
      sessions: [planSession, session({ id: SID.exec2, phase: 'execute', model: 'opus', end_reason: 'harness_exit', tokens_used: 88_000, budget_tokens: 2_000_000, created_ms: -3 * H })], brief: brief(planned), criteria: criteria(planned),
      continuation: continuation(planned, {
        attempts: [planAttempt, attempt({ id: SID.exec2, phase: 'execute', model: 'opus', end_reason: 'harness_exit', tokens_used: 88_000, created_ms: -3 * H })],
        next: { kind: 'change_approach', text: `Session ${SID.exec2.slice(0, 8)} lost its harness. Continue with a different approach, or abandon it.`, session_id: SID.exec2 },
        actions: { continue_from: SID.exec2, abandon: true },
      }),
    }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Abandon' }).click()
      await page.fill('input[aria-label="Why it is abandoned"]', 'superseded by the gateway rewrite')
    },
  },
  {
    id: 'work-item-continue-refused',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Continue refused by the node',
    since: '#385',
    api: itemApi(planned, {
      sessions: [planSession, session({ id: SID.exec1, phase: 'execute', model: 'opus', end_reason: 'node_restart', tokens_used: 512_000, budget_tokens: 2_000_000, created_ms: -6 * H })], brief: brief(planned), criteria: criteria(planned),
      continuation: continuation(planned, {
        attempts: [planAttempt, attempt({ id: SID.exec1, phase: 'execute', model: 'opus', end_reason: 'node_restart', tokens_used: 512_000, created_ms: -6 * H })],
        next: { kind: 'continue', text: `Session ${SID.exec1.slice(0, 8)} was cut off by a node restart; continue from its workspace.`, session_id: SID.exec1 },
        actions: { continue_from: SID.exec1, abandon: true },
      }),
      api: { 'POST /api/continuation/continue': err(409, 'the workspace of 8b4e0d2a was removed by garbage collection; start a fresh execute session') },
    }),
    act: async (page) => page.getByRole('button', { name: 'Continue', exact: true }).click(),
  },
  {
    id: 'work-item-provider-exhausted',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Attempt ended because the provider was exhausted',
    since: '#384',
    note: 'attempt state reads "provider exhausted"; next asks for a change of approach',
    api: itemApi(planned, {
      sessions: [planSession, session({ id: SID.exec1, phase: 'execute', model: 'opus', end_reason: 'provider_exhausted', last_error: 'anthropic: weekly limit reached', tokens_used: 940_000, budget_tokens: 2_000_000, created_ms: -5 * H })], brief: brief(planned),
      continuation: continuation(planned, {
        attempts: [planAttempt, attempt({ id: SID.exec1, phase: 'execute', model: 'opus', end_reason: 'provider_exhausted', last_error: 'anthropic: weekly limit reached', tokens_used: 940_000, created_ms: -5 * H })],
        next: { kind: 'change_approach', text: `Session ${SID.exec1.slice(0, 8)} failed: anthropic: weekly limit reached. Continue with a different approach, or abandon it.`, session_id: SID.exec1 },
        workspace: { id: SID.exec1.slice(0, 12), branch: 'feat/rate-limits', session_id: SID.exec1 },
        actions: { continue_from: SID.exec1, abandon: true },
      }),
    }),
  },
  {
    id: 'work-item-done',
    area: 'work',
    route: `/work/${ID.cookies}`,
    title: 'Closed by its session, published',
    since: '#385',
    note: 'green head, Closed by session line, next "done", published review in evidence, Reopen only',
    api: (() => {
      const it = ledger.find((i) => i.id === ID.cookies)
      return itemApi(it, {
        sessions: [
          session({ id: SID.plan, work_item_id: it.id, phase: 'plan', created_ms: -4 * D }),
          session({ id: SID.exec2, work_item_id: it.id, phase: 'execute', model: 'opus', end_reason: 'item_close', tokens_used: 860_000, budget_tokens: 2_000_000, created_ms: -2 * D }),
          session({ id: SID.review, work_item_id: it.id, phase: 'review', end_reason: 'phase_done', tokens_used: 64_000, budget_tokens: 400_000, created_ms: -30 * H }),
        ],
        discovered: [
          { id: ID.export, title: 'Nightly export of the docs corpus to object storage', state: 'open' },
          { id: ID.docsync, title: 'Sync document edits between peers', state: 'closed' },
        ],
        evidence: evidenceBy({ [SID.exec2]: evidence(it, 1) }),
        continuation: continuation(it, {
          attempts: [
            attempt({ id: SID.plan, created_ms: -4 * D }),
            attempt({ id: SID.exec2, phase: 'execute', model: 'opus', end_reason: 'item_close', tokens_used: 860_000, created_ms: -2 * D }),
            attempt({ id: SID.review, phase: 'review', end_reason: 'phase_done', tokens_used: 64_000, created_ms: -30 * H }),
          ],
          next: { kind: 'done', text: 'The item is closed; nothing is left to do.', session_id: null },
          workspace: { id: SID.review.slice(0, 12), branch: 'feat/session-cookies', session_id: SID.review },
          evidence: {
            reviews: [{ id: 'rv-9', session_id: SID.exec2, title: 'feat(auth): session cookies over remember-me tokens', state: 'approved', verdict_reason: null, publish_result: 'published as #212', head_sha: '19d4f7a3c0e2', created_ms: -30 * H }],
            shown: [],
          },
          actions: { continue_from: null, abandon: false },
        }),
      })
    })(),
  },
  {
    id: 'work-item-abandoned',
    area: 'work',
    route: `/work/${ID.docsync}`,
    title: 'Closed without a session (abandoned)',
    since: '#385',
    api: (() => {
      const it = ledger.find((i) => i.id === ID.docsync)
      return itemApi(it, {
        sessions: [session({ id: SID.exec1, work_item_id: it.id, phase: 'execute', state: 'failed', end_reason: 'error', last_error: 'harness exited with status 137', tokens_used: 40_000, budget_tokens: 400_000, created_ms: -9 * D })],
        continuation: continuation(it, {
          attempts: [attempt({ id: SID.exec1, phase: 'execute', state: 'failed', end_reason: 'error', last_error: 'harness exited with status 137', tokens_used: 40_000, created_ms: -9 * D })],
          next: { kind: 'done', text: 'The item is closed; nothing is left to do.', session_id: null },
          actions: { continue_from: null, abandon: false },
        }),
      })
    })(),
  },
  {
    id: 'work-item-continuation-error',
    area: 'work',
    route: `/work/${ID.export}`,
    title: 'Continuation view unavailable',
    since: '#385',
    api: itemApi(ledger.find((i) => i.id === ID.export), {
      api: { [`/api/work/${ID.export}/continuation`]: err(500, 'reading sessions of the item: database is locked') },
    }),
  },
  {
    id: 'work-item-not-found',
    area: 'work',
    route: '/work/0000deadbeef0000',
    title: 'No such item',
    api: { '/api/work/0000deadbeef0000': err(404, 'no work item 0000deadbeef0000') },
  },
  {
    id: 'work-item-loading',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Item loading',
    api: { [`/api/work/${ID.limits}`]: never },
  },
  {
    id: 'work-item-close-refused',
    area: 'work',
    route: `/work/${ID.export}`,
    title: 'Closing refused',
    note: 'item actions are desktop-only',
    sizes: ['desktop'],
    api: itemApi(ledger.find((i) => i.id === ID.export), {
      api: { [`PUT /api/work/${ID.export}`]: err(409, `session ${SID.run.slice(0, 8)} holds this item; stop it before closing`) },
    }),
    act: async (page) => page.getByRole('button', { name: 'Close', exact: true }).click(),
  },
  {
    id: 'work-item-long',
    area: 'work',
    route: `/work/${ID.long}`,
    title: 'Long title, body, deps, branch and many attempts',
    since: '#385',
    note: 'wrapping of the head, Waits on list, attempt rows, workspace branch and the discovered chain',
    api: (() => {
      const deps = ledger.filter((l) => l.state === 'open' && l.id !== ID.long).map((l) => l.id)
      const it = item({
        id: ID.long,
        title: longTitle,
        body: `${longTitle}.\n\nDone looks like: a review approved on any node publishes once, from the node with the credential.\nMust not touch: the review schema, the mirror protocol.\n\nTrace: ${'/var/lib/tracon/state/sessions/'.repeat(4)}publish.log`,
        priority: 9,
        deps,
        readiness: { state: 'blocked', by: deps.map((id) => ({ kind: 'open', id })) },
        phase_plan_slug: 'plan-rework-publication-path-from-the-credential-holding-node',
        brief_slug: 'brief-rework-publication-path-from-the-credential-holding-node',
        discovered_from: ID.limits,
        discovered_by_session: SID.exec1,
      })
      const attempts = Array.from({ length: 9 }, (_, i) =>
        attempt({
          id: `${i}${SID.exec1.slice(1)}`,
          phase: i === 0 ? 'plan' : 'execute',
          model: i % 2 ? 'claude-opus-4-1-20250805' : 'sonnet',
          end_reason: i === 8 ? 'error' : i === 0 ? 'phase_done' : ['node_restart', 'budget', 'detached', 'killed_user'][i % 4],
          last_error: i === 8 ? `tool call failed: ${longUnbroken}` : null,
          tokens_used: 120_000 * (i + 1),
          continued_from: i > 0 ? `${i - 1}${SID.exec1.slice(1)}` : null,
          created_ms: -(10 - i) * H,
        }),
      )
      return itemApi(it, {
        sessions: attempts.map((a) => session({ id: a.id, work_item_id: it.id, phase: a.phase, end_reason: a.end_reason, tokens_used: a.tokens_used, budget_tokens: 2_000_000, created_ms: a.created_ms })),
        discovered: Array.from({ length: 6 }, (_, i) => ({ id: `${i}c4f9e7a2d61b835`, title: i % 2 ? longTitle : 'Follow-up: retry a publish that timed out', state: i % 3 ? 'open' : 'closed' })),
        continuation: continuation(it, {
          attempts,
          next: { kind: 'unblock', text: `Waits on ${deps.map((d) => `“${titleOf[d].title}” (${d.slice(0, 8)})`).join(', ')}.`, session_id: attempts[8].id },
          workspace: { id: attempts[8].id.slice(0, 12), branch: longUnbroken, session_id: attempts[8].id },
          actions: { continue_from: attempts[8].id, abandon: true },
        }),
      })
    })(),
  },

  // ------------------------------------------------------------ the brief
  {
    id: 'work-item-brief-adding',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Brief: adding an observed line with a bad reference',
    note: 'both validation notices: "Not a reference" and "An observation with nothing to point at"',
    sizes: ['desktop'],
    api: itemApi(planned, { sessions: [planSession], brief: brief(planned), criteria: criteria(planned) }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Add a line' }).click()
      await page.selectOption('select[aria-label="who is behind this line"]', 'observed')
      await page.fill('.add input >> nth=0', 'Two operators asked for a per-channel limit in the ops sync.')
      await page.fill('.add input >> nth=1', 'meeting-ops-sync note:missing')
    },
  },
  {
    id: 'work-item-brief-all-inferred',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Brief with nothing observed, most sections absent',
    note: '"Nothing in this brief was observed" notice, absent sections, the extra Markdown block',
    api: itemApi(planned, {
      sessions: [planSession],
      brief: brief(planned, {
        preamble: '',
        sections: [
          { field: 'problem', heading: 'Problem', notes: 'Drafted by the plan session.', entries: [{ provenance: 'inferred', text: 'Bursts from one channel likely delay others.', refs: [] }] },
          { field: 'success_criteria', heading: 'Success criteria', notes: '', entries: [{ provenance: 'unattributed', text: 'Bursts are smoothed.', refs: [] }] },
        ],
        extra: '## Scratch\n\n- look at the broker queue depth metric\n- ask whether the hub needs the same limiter',
        counts: { observed: 0, inferred: 1, decided: 0, unattributed: 1 },
        absent: [
          { field: 'intended_user', heading: 'Intended user', says: 'no intended user named' },
          { field: 'source_references', heading: 'Source references', says: 'nothing cited' },
          { field: 'constraints', heading: 'Constraints', says: 'no constraints stated' },
          { field: 'unresolved_questions', heading: 'Unresolved questions', says: 'no open questions recorded' },
        ],
      }),
      criteria: criteria(planned, {
        candidate: undefined,
        criteria: [criterion({ key: 'c1', text: 'Bursts are smoothed.', provenance: 'unattributed', standard: 'unattributed' })],
        gaps: { uncovered: ['c1'], unjudged: ['c1'], assumptions: [{ field: 'problem', heading: 'Problem', text: 'Bursts from one channel likely delay others.', provenance: 'inferred' }], questions: [], orphaned_judgements: [] },
        summary: '1 criterion · 1 nothing points at them · 1 not an agreed standard · 1 assumptions',
      }),
    }),
  },
  {
    id: 'work-item-brief-unreadable',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Brief pointer this node cannot read',
    api: itemApi(planned, { sessions: [planSession] }),
  },

  // --------------------------------------------------------- the criteria
  {
    id: 'work-item-criteria-retired',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Criteria: retired scenario/observation links, duplicate, earlier and orphaned verdicts',
    since: '#398',
    note: 'retired links say they settle nothing; duplicate disables its actions; open questions and reworded-verdict sections',
    api: itemApi(planned, {
      sessions: [planSession],
      brief: brief(planned, {
        sections: [
          ...brief(planned).sections,
          {
            field: 'unresolved_questions',
            heading: 'Unresolved questions',
            notes: '',
            entries: [
              { provenance: 'unattributed', text: 'Does the hub need the same limiter, or only nodes?', refs: [] },
              { provenance: 'unattributed', text: 'Is one second the right bound for a client channel?', refs: [] },
            ],
          },
        ],
        counts: { observed: 2, inferred: 2, decided: 2, unattributed: 3 },
        absent: [],
      }),
      criteria: criteria(planned, {
        criteria: [
          criterion({
            key: 'c1',
            links: [
              { index: 0, provenance: 'decided', standard: 'agreed', kind: 'scenario', value: 'burst-on-personal-while-work-runs', refs: [], unresolved: 'scenario links are retired and settle nothing; judge this criterion, or point a configured check at it' },
              { index: 1, provenance: 'observed', standard: 'agreed', kind: 'observation', value: 'operator saw no stall during the 3 Oct incident replay', refs: [], unresolved: 'observation links are retired and settle nothing; judge this criterion, or point a configured check at it' },
            ],
            coverage: 'awaits_judgement',
            earlier_judgement: { id: 'j-0', verdict: 'not_met', note: 'stalled 3s', candidate_id: 'cand-old', criterion_text: 'A burst on one channel never delays another channel by more than a second.', judged_ms: -D },
          }),
          criterion({ key: 'c2', text: 'The limit is configurable per channel in node.toml.', links: [{ index: 0, provenance: 'decided', standard: 'agreed', kind: 'check', value: 'just test', refs: [], outcome: 'running', run_id: 'run-43' }], coverage: 'no_result_yet', refs: [{ kind: 'doc', value: 'guide-node-toml', known: false }] }),
          criterion({ key: 'c3', text: 'The limit is configurable per channel in node.toml.', coverage: 'nothing_points_at_it', duplicate: true }),
          criterion({ key: 'c4', text: 'Operators can see why a call was delayed.', coverage: 'judged_unclear', judgement: { id: 'j-2', verdict: 'unclear', note: 'the log line exists but nobody reads the log', candidate_id: 'cand-7e21', criterion_text: 'Operators can see why a call was delayed.', judged_ms: -20 * M } }),
          criterion({ key: 'c5', text: 'No call is dropped, only delayed.', coverage: 'judged_not_met', judgement: { id: 'j-3', verdict: 'not_met', candidate_id: 'cand-7e21', criterion_text: 'No call is dropped, only delayed.', judged_ms: -10 * M } }),
        ],
        gaps: {
          uncovered: ['c3'],
          unjudged: ['c1', 'c2', 'c3'],
          assumptions: [],
          questions: ['Does the hub need the same limiter, or only nodes?', 'Is one second the right bound for a client channel?'],
          orphaned_judgements: [{ id: 'j-old', verdict: 'met', criterion_text: 'Bursts are smoothed.', judged_ms: -2 * D }],
        },
        summary: '5 criteria · 1 nothing points at them · 1 no result yet · 1 await a person · 1 judged not met · 1 judged unclear · 2 open questions · 1 verdicts on criteria that were reworded',
      }),
    }),
  },
  {
    id: 'work-item-criteria-link-form',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Criteria: say what settles it (check only)',
    since: '#398',
    note: 'the link form no longer offers a kind picker: provenance select and a check command',
    sizes: ['desktop'],
    api: itemApi(planned, { sessions: [planSession], brief: brief(planned), criteria: criteria(planned) }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Say what settles it' }).nth(1).click()
      await page.fill('input[placeholder="the configured check command, exactly"]', 'cargo nextest run -p tracon-node config::')
    },
  },
  {
    id: 'work-item-criteria-judge-form',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Criteria: judging with nothing captured',
    note: 'verdict form with the "about the criterion itself" notice',
    sizes: ['desktop'],
    api: itemApi(planned, {
      sessions: [planSession],
      brief: brief(planned),
      // Nothing captured: no check has a result, and no verdict is bound to an attempt.
      criteria: criteria(planned, {
        candidate: undefined,
        criteria: [
          criterion({ key: 'c1', links: [{ index: 0, provenance: 'decided', standard: 'agreed', kind: 'check', value: 'cargo nextest run -p tracon-node gateway::limits', refs: [] }], coverage: 'no_result_yet' }),
          criterion({ key: 'c2', text: 'The limit is configurable per channel in node.toml.', provenance: 'inferred', standard: 'proposed', coverage: 'nothing_points_at_it' }),
        ],
        gaps: { uncovered: ['c2'], unjudged: ['c1', 'c2'], assumptions: [], questions: [], orphaned_judgements: [] },
        summary: '2 criteria · 1 nothing points at them · 1 no result yet · 1 not an agreed standard',
      }),
    }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Judge it' }).first().click()
      await page.selectOption('select[aria-label="your verdict"]', 'not_met')
      await page.fill('input[placeholder="what you looked at, in one sentence"]', 'read the plan; it never mentions the hub')
    },
  },
  {
    id: 'work-item-criteria-none-stated',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Criteria: brief states none',
    api: itemApi(planned, {
      sessions: [planSession],
      brief: brief(planned, { sections: brief(planned).sections.filter((s) => s.field !== 'success_criteria'), absent: [{ field: 'success_criteria', heading: 'Success criteria', says: 'no success criteria stated' }] }),
      criteria: criteria(planned, { criteria: [], absent: 'no success criteria stated', gaps: { uncovered: [], unjudged: [], assumptions: [], questions: [], orphaned_judgements: [] }, summary: 'no success criteria stated' }),
    }),
  },

  // ---------------------------------------------------------- the context
  {
    id: 'work-item-context-attempts-open',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Context: each attempt expanded',
    note: 'changes and per-document delivery (full, cut short, left out)',
    api: itemApi(planned, { sessions: [planSession], brief: brief(planned), criteria: criteria(planned), context: contextFull(planned) }),
    act: openDetails('.context details'),
  },
  {
    id: 'work-item-context-adding',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Context: adding a document',
    sizes: ['desktop'],
    api: itemApi(planned, { sessions: [planSession], brief: brief(planned), criteria: criteria(planned), context: contextFull(planned) }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Add a document' }).click()
      await page.fill('.context .add input >> nth=0', 'ref-gateway-incident-2026-10-03')
      await page.fill('.context .add input >> nth=1', 'the incident this item came from')
    },
  },
  {
    id: 'work-item-context-empty-selection',
    area: 'work',
    route: `/work/${ID.limits}`,
    title: 'Context: selection document with no picks',
    api: itemApi(planned, {
      sessions: [planSession], brief: brief(planned), criteria: criteria(planned),
      context: { ...contextFull(planned), selection: { ...contextFull(planned).selection, picks: [], resolved: [] }, attempts: [] },
    }),
  },

  // ------------------------------------------------- launching from an item
  {
    id: 'work-launch-plan',
    area: 'work',
    route: `/?item=${ID.export}&phase=plan`,
    title: 'Launch a plan from an item, adjust open',
    since: '#387',
    note: 'item header in the composer, readiness line (all ready), preparation line under the repository, phase segment',
    api: launchApi(ledger.find((i) => i.id === ID.export)),
    act: async (page) => {
      await openAdjust(page)
      await openDetails('details.ready')(page)
    },
  },
  {
    id: 'work-launch-needs-plan',
    area: 'work',
    route: `/?item=${ID.export}&phase=execute`,
    title: 'Execute asked of an item with no plan',
    note: 'Start disabled; the crit line under Phase',
    api: launchApi(ledger.find((i) => i.id === ID.export)),
    act: openAdjust,
  },
  {
    id: 'work-launch-needs-plan-collapsed',
    area: 'work',
    route: `/?item=${ID.export}&phase=execute`,
    title: 'Execute asked of an item with no plan, composer collapsed',
    note: 'Start is disabled; is the reason visible without opening adjust?',
    api: launchApi(ledger.find((i) => i.id === ID.export)),
  },
  {
    id: 'work-launch-readiness-gaps',
    area: 'work',
    route: `/?item=${ID.limits}&phase=execute`,
    title: 'Launch: verify and publish not ready',
    since: '#387',
    note: 'the ✓/✗ summary and each path listing only what it adds; notes styled apart from gaps',
    api: launchApi(planned, {
      readiness: {
        channel: 'personal',
        repo: '/home/op/src/orbit',
        investigate: { purpose: 'investigate', ready: true, missing: [], notes: [] },
        verify: {
          purpose: 'verify',
          ready: false,
          missing: [{ key: 'checks', message: 'no required checks are configured for this repository, so nothing the node runs can verify a result; add `checks` to its [[repo]] entry' }],
          notes: [{ key: 'image', message: 'checks run in the node\'s harness image (ghcr.io/cosmicspork/tracon-harness-claude:2.5.0); the repository names no toolchain of its own' }],
        },
        publish: {
          purpose: 'publish',
          ready: false,
          missing: [
            { key: 'checks', message: 'no required checks are configured for this repository, so nothing the node runs can verify a result; add `checks` to its [[repo]] entry' },
            { key: 'credential', message: 'GitHub credential `github`: not bound to channel personal on this node' },
          ],
          notes: [],
        },
      },
    }),
    act: openDetails('details.ready'),
  },
  {
    id: 'work-launch-not-investigable',
    area: 'work',
    route: `/?item=${ID.limits}&phase=execute`,
    title: 'Launch: nothing ready (repository missing, node not ready)',
    since: '#387',
    api: launchApi(planned, {
      readiness: {
        channel: 'personal',
        repo: '/home/op/src/orbit',
        investigate: {
          purpose: 'investigate',
          ready: false,
          missing: [
            { key: 'repo', message: '/home/op/src/orbit is not a directory on this node' },
            { key: 'launch', message: 'this node is degraded: the gateway container is not running' },
          ],
          notes: [],
        },
        verify: { purpose: 'verify', ready: false, missing: [{ key: 'repo', message: '/home/op/src/orbit is not a directory on this node' }, { key: 'launch', message: 'this node is degraded: the gateway container is not running' }], notes: [] },
        publish: {
          purpose: 'publish',
          ready: false,
          missing: [
            { key: 'repo', message: '/home/op/src/orbit is not a directory on this node' },
            { key: 'launch', message: 'this node is degraded: the gateway container is not running' },
            { key: 'remote', message: 'the repository has no `origin` remote to publish to' },
          ],
          notes: [{ key: 'brief', message: 'the work item has no brief, so its review shows no requirements to judge against' }],
        },
      },
    }),
    act: openDetails('details.ready'),
  },
  {
    id: 'work-launch-preparation-incompatible',
    area: 'work',
    route: `/?item=${ID.limits}&phase=execute`,
    title: 'Launch: preparation would refuse the devcontainer',
    since: '#388',
    note: 'preparation line, crit summary when blocking, each incompatibility with where its work belongs',
    api: launchApi(planned, {
      preparation: {
        repo: '/home/op/src/orbit',
        image: 'mcr.microsoft.com/devcontainers/rust:1-bookworm',
        image_source: 'repository devcontainer',
        devcontainer_image: 'mcr.microsoft.com/devcontainers/rust:1-bookworm',
        lockfiles: ['Cargo.lock', 'package-lock.json'],
        install: 'cargo fetch --locked',
        prepare: [],
        egress: [],
        incompatible: [
          { source: '.devcontainer/devcontainer.json', item: 'image', reason: 'mcr.microsoft.com/devcontainers/rust:1-bookworm is neither pinned to a digest nor in the node\'s approved images', instead: 'the image by digest (`name@sha256:…`), or the [[repo]] entry\'s `image`', blocking: true },
          { source: '.devcontainer/devcontainer.json', item: 'postCreateCommand', reason: 'is a setup hook the repository controls; the node does not run it', instead: "a command in the [[repo]] entry's `prepare`, which runs in the check image with only `egress` reachable", blocking: false },
          { source: '.devcontainer/devcontainer.json', item: 'mounts', reason: 'asks for host paths in the container; the node mounts only the workspace and its cache', instead: null, blocking: false },
          { source: 'package.json', item: 'scripts.postinstall', reason: 'runs during install; preparation installs with scripts off, so it does not run', instead: "a command in the [[repo]] entry's `prepare`, if the project needs it", blocking: false },
        ],
        ready: false,
      },
    }),
    act: async (page) => {
      await openAdjust(page)
      await openDetails('.prep details')(page)
    },
  },
  {
    id: 'work-launch-start-refused',
    area: 'work',
    route: `/?item=${ID.limits}&phase=execute`,
    title: 'Launch refused by the node',
    api: launchApi(planned, {
      api: { 'POST /api/sessions': err(409, `session ${SID.run.slice(0, 8)} already holds work item ${ID.limits.slice(0, 8)}`) },
    }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Start execute' }).click()
    },
  },
]

// An act that fills a field scrolls the page; the shot is taken from the top
// so the sticky sidebar is not caught halfway down a full-page capture.
export default states.map((st) =>
  st.act
    ? {
        ...st,
        act: async (page, ctx) => {
          await st.act(page, ctx)
          await page.evaluate(() => window.scrollTo(0, 0))
        },
      }
    : st,
)
