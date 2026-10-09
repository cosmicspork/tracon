// Evidence, Usage, Nodes and Memories: the screens that report what the node
// observed rather than drive work. api.json has no fixture for metrics,
// memories, evidence or the data inventory, and its /api/mesh predates most of
// MeshState's fields, so every state here brings its own.

const SELF = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
const POD = '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1'
const DESK = 'c7d21e5fa09b3846e1d0c2b7a5f48e3d6c1b0a9f8e7d6c5b4a3f2e1d0c9b8a7f'
const CI = '1e0f9d8c7b6a5f4e3d2c1b0a9f8e7d6c5b4a3f2e1d0c9b8a7f6e5d4c3b2a1f0e'
const NAS = '5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b'
const OLD = 'e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4'

/** A deterministic 40-hex commit sha from a small seed. */
const sha = (seed) => {
  let x = (seed * 2654435761) >>> 0
  let out = ''
  while (out.length < 40) {
    x = (x * 1103515245 + 12345) >>> 0
    out += x.toString(16).padStart(8, '0')
  }
  return out.slice(0, 40)
}

const err = (status, message) => ({ status, body: { error: { code: status, message } } })
const hang = () => new Promise(() => {})

// ---------------------------------------------------------------- mesh / nodes

const meshBase = {
  hub_url: 'https://hub.example.net',
  node_id: SELF,
  fingerprint: '9f31:c6a8:70d2:4b5e',
  last_ok_ms: -8000,
  queued: 0,
  delivered_since_reconnect: 0,
  undecryptable: 0,
  held: 0,
  last_error: null,
  last_refusal: null,
}
const meshConnected = { ...meshBase, hub: { state: 'connected' } }
const meshDisabled = {
  ...meshBase,
  hub: { state: 'disabled' },
  hub_url: null,
  last_ok_ms: null,
}
const meshUnreachable = {
  ...meshBase,
  hub: { state: 'unreachable', since_ms: -1_860_000 },
  last_ok_ms: -1_860_000,
  queued: 14,
  last_error: 'connect to hub.example.net:443: connection timed out',
}

const anthropic = (channels, extra = {}) => ({
  name: 'anthropic',
  state: 'connected',
  kind: 'oauth',
  can_login: true,
  url: null,
  error: null,
  identity: 'op@example.com',
  expires_ms: 5_400_000,
  channels,
  updated_ms: -3_600_000,
  ...extra,
})

const models = [
  { value: 'anthropic/sonnet', name: 'Sonnet', harnesses: ['claude', 'opencode'] },
  { value: 'anthropic/opus', name: 'Opus', harnesses: ['claude', 'opencode'] },
]

const harness = (id, pinned, found, extra = {}) => ({
  id,
  pinned,
  found,
  mismatch: found !== pinned,
  default: id === 'claude',
  image: `ghcr.io/example/tracon-${id}:${pinned}`,
  image_state: 'current',
  ...extra,
})

const node = (o) => {
  const harnesses = o.harnesses ?? [harness('claude', '2.5.0', '2.5.0'), harness('opencode', '1.4.2', '1.4.2')]
  const first = harnesses[0] ?? { id: 'claude', pinned: '2.5.0', found: null, mismatch: false }
  return {
    id: o.id,
    name: o.name,
    state: o.state ?? 'ready',
    failed_check: o.failed_check ?? null,
    failed_detail: o.failed_detail ?? null,
    harness: { id: first.id, pinned: first.pinned, found: first.found, mismatch: first.mismatch },
    harnesses,
    models: o.models ?? models,
    checked_at_ms: o.checked_at_ms ?? -420_000,
    is_self: o.is_self ?? false,
    reachable: o.reachable ?? true,
    last_seen_ms: o.last_seen_ms ?? (o.is_self ? null : -22_000),
    providers: o.providers ?? [anthropic(['personal', 'work'])],
    application_version: o.application_version ?? '0.29.0',
    ...(o.is_self ? { loopback: true, default_channel: 'personal' } : {}),
  }
}

const self = node({ id: SELF, name: 'laptop', is_self: true })
const pod = node({ id: POD, name: 'work-pod', providers: [anthropic(['work'])] })

const awakeIdle = { held: false, reason: null, method: 'logind', error: null, last_suspend: null }

const data = (total) => ({
  database_bytes: Math.round(total * 0.18),
  total_bytes: total,
  holdings: [
    { kind: 'sessions', label: 'Sessions', unit: 'session', count: 214, bytes: Math.round(total * 0.06), delete: null, propagation: 'Nothing deletes a session yet.' },
    { kind: 'events', label: 'Session logs', unit: 'event', count: 182_440, bytes: Math.round(total * 0.09), delete: null, propagation: 'Nothing deletes a log entry yet.' },
    { kind: 'workspaces', label: 'Workspaces', unit: 'workspace', count: 9, bytes: Math.round(total * 0.67), delete: { path: '/settings#maintenance', label: 'Maintenance, under Runtime storage, once the session is archived' }, propagation: 'Local to this node.' },
  ],
})

/** The calls the Nodes screen makes, with overrides on top. */
const nodesApi = (o = {}) => ({
  '/api/node': o.self ?? self,
  '/api/nodes': o.nodes ?? [self, pod],
  '/api/mesh': o.mesh ?? meshConnected,
  '/api/awake': o.awake ?? awakeIdle,
  '/api/maintenance/data': o.data ?? data(3_412_000_000),
  ...(o.queue ? { '/api/queue': o.queue } : {}),
  ...(o.channels ? { '/api/channels': o.channels } : {}),
})

const session = (id, nodeId, extra = {}) => ({
  id,
  node_id: nodeId,
  channel: 'work',
  work_item_id: null,
  repo_path: '/home/op/src/orbit',
  worktree_path: `/tmp/orbit-${id}`,
  branch: `feat/${id}`,
  harness_id: 'claude',
  harness_version: '2.5.0',
  model: 'anthropic/sonnet',
  phase: 'execute',
  policy_version: 12,
  review_id: null,
  budget_tokens: 2_000_000,
  tokens_used: 180_000,
  cost_usd: 1.12,
  context_used: 60_000,
  context_size: 200_000,
  state: 'running',
  end_reason: null,
  last_error: null,
  turn_active: 1,
  draft: null,
  created_ms: -1_200_000,
  updated_ms: -30_000,
  archived_ms: null,
  ...extra,
})

const permission = (id, sessionId, nodeId) => ({
  id,
  session_id: sessionId,
  node_id: nodeId,
  title: 'Bash: cargo nextest run --workspace',
  kind: 'execute',
  raw_input: '{"command":"cargo nextest run --workspace","cwd":"/work"}',
  options: '[{"option_id":"allow_once","name":"Allow","kind":"allow_once"},{"option_id":"reject_once","name":"Deny","kind":"reject_once"}]',
  state: 'new',
  created_ms: -90_000,
  expires_ms: 810_000,
})

// ---------------------------------------------------------------- evidence

const candidate = (n, o = {}) => {
  const head = o.head ?? sha(n)
  const channel = o.channel ?? 'personal'
  return {
    candidate: {
      id: `${head}:${channel}`,
      head_sha: head,
      channel,
      owner_session_id: o.session ?? `s-${n}`,
      owner_node_id: o.owner ?? SELF,
      source_kind: 'git',
      captured_ms: o.captured ?? -(n * 3_100_000 + 240_000),
    },
    work_items: o.work ?? [],
    more_work_items: o.moreWork ?? false,
    reviews: o.reviews ?? [],
    more_reviews: o.moreReviews ?? false,
  }
}

const candidates = [
  candidate(1, {
    work: [{ id: 'wi-2', title: 'Rate-limit the public export endpoint', state: 'open' }],
    reviews: [{ id: 'r-1', title: 'feat(api): rate-limit /export per token', state: 'new' }],
  }),
  candidate(2, {
    work: [{ id: 'wi-7', title: 'Retry webhook delivery with backoff', state: 'closed' }],
    reviews: [{ id: 'r-4', title: 'fix(webhooks): back off on 5xx instead of dropping', state: 'published' }],
  }),
  candidate(3, {
    channel: 'work',
    owner: POD,
    work: [],
    reviews: [{ id: 'rev-1', title: 'Export queue metrics per channel', state: 'new' }],
  }),
  candidate(4, { work: [{ id: 'wi-11', title: 'Document the session ceiling in the operator guide', state: 'open' }] }),
  candidate(5, {}),
  candidate(6, {
    reviews: [
      { id: 'r-9', title: 'refactor(store): split the evidence queries out of store.rs', state: 'rejected' },
      { id: 'r-10', title: 'refactor(store): split evidence queries (second attempt)', state: 'approved' },
    ],
  }),
]

const page = (items, next = null) => ({ items, next_before: next })

const detail = (c) => ({
  candidate: {
    id: c.candidate.id,
    channel: c.candidate.channel,
    head_sha: c.candidate.head_sha,
    owner_session_id: c.candidate.owner_session_id,
    owner_node_id: c.candidate.owner_node_id,
    captured_ms: c.candidate.captured_ms,
  },
})

const longTitle =
  'Replace the hand-rolled retry loop in the forge client with the shared backoff policy, keep the 429 Retry-After handling, and stop retrying 422 validation failures that will never succeed'
const longCandidates = [
  candidate(21, {
    channel: 'client-hounddog-reading-platform',
    work: [
      { id: 'wi-301', title: longTitle, state: 'open' },
      ...Array.from({ length: 11 }, (_, i) => ({
        id: `wi-${310 + i}`,
        title: `Follow-up ${i + 1}: migrate ${['lesson', 'roster', 'grade', 'assignment', 'reader', 'invite', 'billing', 'audit', 'export', 'import', 'sso'][i]} endpoints to the new rate limiter`,
        state: i % 3 ? 'open' : 'closed',
      })),
    ],
    moreWork: true,
    reviews: [
      { id: 'r-301', title: `feat(forge): ${longTitle}`, state: 'new' },
      { id: 'r-302', title: 'chore: bump the forge client to the backoff crate', state: 'published' },
    ],
    moreReviews: true,
  }),
  candidate(22, {
    channel: 'client-hounddog-reading-platform',
    owner: DESK,
    reviews: [
      {
        id: 'r-303',
        title: 'fix(paths): handle /var/home/op/src/.worktrees/hounddogreading-fix-very-long-branch-name-for-the-audit/app/Http/Controllers/Api/V2/LessonAssignmentExportController.php',
        state: 'new',
      },
    ],
  }),
  candidate(23, {
    work: [{ id: 'wi-330', title: 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-unbroken', state: 'open' }],
  }),
]

const evidenceApi = (list, extra = {}) => ({
  '/api/nodes': [self, pod, node({ id: DESK, name: 'desk' })],
  '/api/channels': channelsWithDesk,
  '/api/evidence/candidates': list,
  ...extra,
})

const channelsWithDesk = [
  {
    name: 'personal',
    nodes: [SELF, POD, DESK],
    bindings: { phases: { plan: { model: 'sonnet' }, execute: { model: 'opus' } } },
    ceiling: { usage_today: 412_000, ceiling: 4_000_000, state: 'under' },
    archived: null,
  },
  {
    name: 'work',
    nodes: [POD],
    bindings: { phases: { review: { model: 'opus' } } },
    ceiling: { usage_today: 1_660_000, ceiling: 2_000_000, state: 'near' },
    archived: null,
  },
  {
    name: 'client-hounddog-reading-platform',
    nodes: [SELF, DESK],
    bindings: {},
    ceiling: { usage_today: 0, ceiling: null, state: 'under' },
    archived: null,
  },
]

const selectedRoute = (c) =>
  `/evidence?candidate=${encodeURIComponent(c.candidate.id)}&channel=${c.candidate.channel}${c.candidate.owner_node_id !== SELF ? `&owner=${c.candidate.owner_node_id}` : ''}`

// ---------------------------------------------------------------- metrics

const metric = (channel, o = {}) => ({
  channel,
  since_ms: -30 * 86_400_000,
  accepted_changes: 0,
  rejected_changes: 0,
  approvals: 0,
  approvals_per_accepted_change: null,
  tokens_per_accepted_change: null,
  tokens: 0,
  cost_usd: null,
  human_seconds: 0,
  agent_seconds: 0,
  sessions: 0,
  setup_failures: 0,
  verified_sessions: 0,
  seconds_to_first_verified_candidate: null,
  interventions: 0,
  question_wait_seconds: 0,
  human_wait_seconds: 0,
  ...o,
})

const NOTE = 'as seen from this node: usage is counted where the model call was made'
const metricsBody = (channels) => ({ since_ms: -30 * 86_400_000, node_id: SELF, note: NOTE, channels })

const typicalMetrics = [
  metric('personal', {
    accepted_changes: 23,
    rejected_changes: 4,
    approvals: 61,
    approvals_per_accepted_change: 2.652,
    tokens_per_accepted_change: 1_184_000,
    tokens: 31_400_000,
    cost_usd: 84.37,
    human_seconds: 14_820,
    agent_seconds: 162_300,
    sessions: 41,
    setup_failures: 2,
    verified_sessions: 27,
    seconds_to_first_verified_candidate: 2_740,
    interventions: 88,
    question_wait_seconds: 3_100,
    human_wait_seconds: 21_900,
  }),
  metric('work', {
    accepted_changes: 9,
    rejected_changes: 1,
    approvals: 14,
    approvals_per_accepted_change: 1.556,
    tokens_per_accepted_change: 742_000,
    tokens: 8_020_000,
    cost_usd: null,
    human_seconds: 3_960,
    agent_seconds: 41_200,
    sessions: 12,
    setup_failures: 0,
    verified_sessions: 10,
    seconds_to_first_verified_candidate: 1_510,
    interventions: 19,
    question_wait_seconds: 600,
    human_wait_seconds: 5_400,
  }),
]

const manyMetrics = [
  ...typicalMetrics,
  metric('client-hounddog-reading-platform', {
    accepted_changes: 1_204,
    rejected_changes: 377,
    approvals: 9_812,
    approvals_per_accepted_change: 8.149,
    tokens_per_accepted_change: 12_480_000,
    tokens: 15_026_000_000,
    cost_usd: 48_211.9,
    human_seconds: 1_123_200,
    agent_seconds: 9_904_800,
    sessions: 2_311,
    setup_failures: 148,
    verified_sessions: 1_877,
    seconds_to_first_verified_candidate: 412_000,
    interventions: 18_220,
    question_wait_seconds: 302_000,
    human_wait_seconds: 3_801_600,
  }),
  metric('kritee'),
  metric('svastha', { sessions: 3, setup_failures: 3, tokens: 1_200, agent_seconds: 40 }),
  metric('tabla', { sessions: 2, tokens: 88_000, agent_seconds: 1_900, verified_sessions: 0, interventions: 4, human_wait_seconds: 45 }),
  metric('homelab-gitops-renovate-and-flux-reconciliation', {
    accepted_changes: 4,
    approvals: 4,
    approvals_per_accepted_change: 1,
    tokens_per_accepted_change: 95_000,
    tokens: 380_000,
    cost_usd: 0.91,
    human_seconds: 300,
    agent_seconds: 2_400,
    sessions: 4,
    verified_sessions: 4,
    seconds_to_first_verified_candidate: 380,
    interventions: 4,
    human_wait_seconds: 1_200,
  }),
]

// ---------------------------------------------------------------- memories

const memory = (n, o = {}) => ({
  id: o.id ?? `m-${n}`,
  channel: o.channel ?? 'personal',
  scope: o.scope ?? 'project',
  scope_ref: o.scope_ref === undefined ? (o.scope ?? 'project') === 'global' ? null : `p-7c41e2a9${n}` : o.scope_ref,
  kind: o.kind ?? 'fact',
  body: o.body,
  source_session: o.source_session ?? `s-${n}`,
  source_node: o.source_node ?? SELF,
  confidence: o.confidence ?? 0.8,
  state: o.state ?? 'active',
  site: SELF,
  hlc_ms: o.updated ?? -n * 7_200_000,
  deleted: 0,
  created_ms: o.created ?? -n * 86_400_000,
  updated_ms: o.updated ?? -n * 7_200_000,
})

const typicalMemories = [
  memory(1, { kind: 'directive', scope: 'global', confidence: 1, body: 'Work through a PR rather than committing to main, even on solo repositories.', state: 'promoted' }),
  memory(2, { kind: 'fact', body: 'orbit runs its integration tests with `just test`; they need the Postgres service from the devcontainer.' }),
  memory(3, { kind: 'lesson', confidence: 0.65, body: 'The forge client retries 422s forever if the payload is invalid; check the validation message before retrying.' }),
  memory(4, { kind: 'episode', scope: 'session', confidence: 0.5, body: 'Rate-limit work stalled on a flaky clock test; pinned the clock with `tokio::time::pause` and it passed.' }),
  memory(5, { kind: 'fact', confidence: 0.9, body: 'Release-please reads only the PR title, so a feat buried under a chore title produces no minor release.', state: 'promoted' }),
  // Not live: the screen must leave these out.
  memory(6, { kind: 'fact', body: 'CANDIDATE — should not render.', state: 'candidate' }),
  memory(7, { kind: 'lesson', body: 'PROPOSED — should not render.', state: 'proposed' }),
  memory(8, { kind: 'fact', body: 'REJECTED — should not render.', state: 'rejected' }),
]

const workMemories = [
  memory(11, { channel: 'work', kind: 'fact', body: 'platform/orbit deploys from main through Flux; never kubectl apply.' }),
  memory(12, { channel: 'work', kind: 'directive', scope: 'global', confidence: 1, body: 'Ask before touching the billing service schema.' }),
]

const longMemories = [
  memory(21, {
    kind: 'lesson',
    body:
      'When the gateway container is restarted by its user service, sessions that were mid-turn lose their egress for up to forty seconds; the harness reports this as a network error and the agent tends to conclude the package registry is down. Wait for the gateway health check before retrying, and do not switch the lockfile to a mirror because of it. The same applies after a node upgrade, when the Podman gateway is started fresh and pulls its image.',
  }),
  memory(22, {
    kind: 'fact',
    body: 'Build output path: /var/home/op/.local/state/tracon/cache/repos/7c41e2a9f0b3d5e6a7c8b9d0e1f2a3b4c5d6e7f8/target/x86_64-unknown-linux-musl/release/build/sqlite-vec-0a1b2c3d4e5f6a7b/out/libsqlite_vec0.a',
  }),
  memory(23, {
    kind: 'directive',
    scope: 'global',
    confidence: 1,
    body: 'Checklist for a release PR:\n1. CI green on the current head\n2. No conflicts with main\n3. The approval names the release being cut\n\nNever merge on a stale approval.',
  }),
  memory(24, {
    kind: 'fact',
    body: 'sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a089f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08',
  }),
  memory(25, { kind: 'a-very-long-custom-memory-kind-name', confidence: 0.333, body: 'An unusual kind should not push the body off the row.' }),
]

const manyMemories = Array.from({ length: 40 }, (_, i) =>
  memory(100 + i, {
    kind: ['fact', 'lesson', 'directive', 'episode'][i % 4],
    scope: i % 5 === 0 ? 'global' : i % 7 === 0 ? 'session' : 'project',
    confidence: [0.95, 0.8, 0.6, 1, 0.45][i % 5],
    body: [
      'Migrations run with `php artisan migrate --pretend` in review before they run for real.',
      'The SPA fixture plugin answers 404 with a JSON error body for any path it has no fixture for.',
      'Pager relay images are tagged by release-please; Renovate in homelab opens the bump.',
      'Prefer Pest datasets over loops in tests; the failure names the case.',
      'The hub drops a frame it cannot decrypt only after the key it waits on is rotated away.',
      'cargo-nextest is on PATH from Homebrew; rustup is keg-only.',
      'Docs slugs are <type>-<name>; formal documents need the operator to agree first.',
    ][i % 7],
    updated: -(i + 1) * 3_600_000,
  }),
)

const memoriesApi = (byChannel) => ({
  '/api/memories': (req) => ({ memories: byChannel[req.query.channel] ?? [] }),
})

// ---------------------------------------------------------------- states

export default [
  // ---- Evidence
  {
    id: 'insight-evidence-populated',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: captured candidates',
    note: 'Cards with task/review links, one owned by a peer (work-pod suffix), and a Load more button.',
    api: evidenceApi(page(candidates, { captured_ms: -19_000_000, id: candidates[5].candidate.id })),
  },
  {
    id: 'insight-evidence-loading',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: first page loading',
    api: evidenceApi(hang),
  },
  {
    id: 'insight-evidence-empty',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: nothing captured',
    api: evidenceApi(page([])),
  },
  {
    id: 'insight-evidence-filter-empty',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: search matches nothing',
    note: 'Empty state with Clear filters inline.',
    api: evidenceApi((req) => page(req.query.q ? [] : candidates)),
    act: async (page) => {
      await page.getByLabel('Search task or review evidence').fill('pagination cursor')
      await page.getByRole('button', { name: 'Search evidence' }).click()
    },
  },
  {
    id: 'insight-evidence-error',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: list fails',
    api: evidenceApi(err(500, 'database is locked')),
  },
  {
    id: 'insight-evidence-remote-runner',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: shared channel, peer runner, peer unreachable',
    note: 'Channel and runner selects filled; the peer read fails with the node message.',
    api: evidenceApi((req) =>
      req.query.runner ? err(502, 'the evidence runner did not answer through the hub within 10s') : page(candidates),
    ),
    act: async (page) => {
      await page.getByLabel('Evidence channel').selectOption('personal')
      await page.getByLabel('Evidence runner').selectOption(DESK)
      await page.getByRole('button', { name: 'Search evidence' }).click()
    },
  },
  {
    id: 'insight-evidence-long',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: long titles, many links, long channel',
    note: 'Ellipsis on long titles and links, the "omitted" note, an unbroken title, a long path title.',
    api: evidenceApi(page(longCandidates)),
  },
  {
    id: 'insight-evidence-load-more',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: second page appended, no more pages',
    api: evidenceApi((req) =>
      req.query.before_ms
        ? page(Array.from({ length: 4 }, (_, i) => candidate(40 + i, { work: [{ id: `wi-${40 + i}`, title: `Older task ${i + 1}`, state: 'closed' }] })))
        : page(candidates, { captured_ms: -19_000_000, id: candidates[5].candidate.id }),
    ),
    act: async (page) => {
      await page.getByRole('button', { name: 'Load more evidence' }).click()
    },
  },
  {
    id: 'insight-evidence-selected',
    area: 'insight',
    route: selectedRoute(candidates[0]),
    title: 'Evidence: a candidate opened',
    note: 'Selected card border, and the candidate summary at the bottom of the page.',
    api: evidenceApi(page(candidates), {
      '/api/evidence/candidates/*/summary': detail(candidates[0]),
    }),
  },
  {
    id: 'insight-evidence-selected-remote',
    area: 'insight',
    route: selectedRoute(candidates[2]),
    title: 'Evidence: a peer-owned candidate opened, no owner session',
    note: 'Owner names the peer; "Missing owner session" where the session id is null.',
    api: evidenceApi(page(candidates), {
      '/api/evidence/candidates/*/summary': { candidate: { ...detail(candidates[2]).candidate, owner_session_id: null } },
    }),
  },
  {
    id: 'insight-evidence-selected-not-found',
    area: 'insight',
    route: '/evidence?candidate=0123456789abcdef0123456789abcdef01234567%3Apersonal',
    title: 'Evidence: technical lookup of an unknown candidate',
    api: evidenceApi(page(candidates), {
      '/api/evidence/candidates/*/summary': err(404, 'candidate was not found'),
    }),
  },
  {
    id: 'insight-evidence-technical-open',
    area: 'insight',
    route: '/evidence',
    title: 'Evidence: technical lookup expanded and filled',
    api: evidenceApi(page(candidates.slice(0, 2))),
    act: async (page) => {
      await page.getByText('Technical candidate lookup').click()
      await page.getByPlaceholder('Candidate id (commit:channel)').fill(`${sha(77)}:personal`)
    },
  },

  // ---- Usage
  {
    id: 'insight-metrics-populated',
    area: 'insight',
    route: '/metrics',
    title: 'Usage: two channels',
    note: 'Cards, table, unpriced cost, the node note under the heading.',
    api: { '/api/metrics': metricsBody(typicalMetrics) },
  },
  {
    id: 'insight-metrics-loading',
    area: 'insight',
    route: '/metrics',
    title: 'Usage: request still in flight',
    note: 'Says it is loading, not that no usage is recorded.',
    api: { '/api/metrics': hang },
  },
  {
    id: 'insight-metrics-empty',
    area: 'insight',
    route: '/metrics',
    title: 'Usage: nothing recorded in the window',
    api: { '/api/metrics': metricsBody([]) },
  },
  {
    id: 'insight-metrics-error',
    area: 'insight',
    route: '/metrics',
    title: 'Usage: request fails',
    api: { '/api/metrics': err(500, 'metrics query failed: no such column: turn_usage.awake_ms') },
  },
  {
    id: 'insight-metrics-many',
    area: 'insight',
    route: '/metrics',
    title: 'Usage: many channels, huge and zero values',
    note: 'Long channel name in card heading and first column, billions of tokens, 1000h+ durations, all-null row.',
    api: { '/api/metrics': metricsBody(manyMetrics) },
  },
  {
    id: 'insight-metrics-7d-methodology',
    area: 'insight',
    route: '/metrics',
    title: 'Usage: 7-day window, methodology open',
    note: 'Active window underline on 7d; the expanded explanation.',
    api: {
      '/api/metrics': (req) =>
        metricsBody(Number(req.query.since_ms) > Date.now() - 8 * 86_400_000 ? [metric('personal', { ...typicalMetrics[0], accepted_changes: 5, sessions: 9, tokens: 6_900_000, cost_usd: 18.02 })] : typicalMetrics),
    },
    act: async (page) => {
      await page.getByRole('button', { name: '7d', exact: true }).click()
      await page.getByText('How Usage is calculated').click()
    },
  },

  // ---- Nodes
  {
    id: 'insight-nodes-single',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: this machine only, no hub',
    since: '#402',
    note: '"holds 3.4 GB" link on the serving node; heading says no hub configured; rail foot "pair a hub".',
    api: nodesApi({ nodes: [self], mesh: meshDisabled }),
  },
  {
    id: 'insight-nodes-hub-connected',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: hub connected, one peer',
    since: '#402',
    note: 'Peer rows never show "holds"; running/awaiting counts per node.',
    api: nodesApi({}),
  },
  {
    id: 'insight-nodes-hub-unreachable',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: hub unreachable, peers stale',
    note: 'Dim banner, rail foot "hub down 31m", peers dimmed with last seen.',
    api: nodesApi({
      mesh: meshUnreachable,
      nodes: [
        self,
        { ...pod, reachable: false, last_seen_ms: -1_900_000 },
        node({ id: DESK, name: 'desk', reachable: false, last_seen_ms: -2_400_000 }),
      ],
    }),
  },
  {
    id: 'insight-nodes-several',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: several peers in every state',
    note: 'Ready, offline, isolation refused, runtime mismatch, isolation unknown, no model offered (provider disconnected).',
    api: nodesApi({
      nodes: [
        self,
        pod,
        node({ id: DESK, name: 'desk', reachable: false, last_seen_ms: -3 * 86_400_000 }),
        node({
          id: CI,
          name: 'ci-runner',
          state: 'refused',
          failed_check: 'gateway',
          failed_detail: 'the Podman gateway user service is not running and could not be started',
        }),
        node({
          id: NAS,
          name: 'nas',
          harnesses: [harness('claude', '2.5.0', '2.3.1'), harness('opencode', '1.4.2', null, { image_state: 'missing' })],
        }),
        node({ id: OLD, name: 'old-mac', state: 'unknown', checked_at_ms: null }),
        node({
          id: 'a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9',
          name: 'spare',
          models: [],
          providers: [anthropic([], { state: 'disconnected', kind: null, identity: null, expires_ms: null, channels: [] })],
        }),
      ],
      queue: {
        waiting: [permission('perm-a', 's-a', POD), permission('perm-b', 's-b', POD)],
        reviews: [],
        promotions: [],
        running: [session('s-a', POD), session('s-b', POD), session('s-c', SELF, { channel: 'personal' })],
        ended: [],
      },
    }),
  },
  {
    id: 'insight-nodes-mismatch',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: one harness mismatched, the other fine',
    note: 'Warn colouring when only one of two harnesses mismatches; readiness still Ready.',
    api: nodesApi({
      nodes: [
        self,
        node({ id: POD, name: 'work-pod', harnesses: [harness('claude', '2.5.0', '2.4.9'), harness('opencode', '1.4.2', '1.4.2')] }),
      ],
    }),
  },
  {
    id: 'insight-nodes-self-refused',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: the serving node failed its isolation check',
    note: 'Crit banner with technical details, red serving-node row.',
    api: (() => {
      const refused = node({
        id: SELF,
        name: 'laptop',
        is_self: true,
        state: 'refused',
        failed_check: 'rootless',
        failed_detail: 'podman info reports rootless=false; the boundary needs a rootless runtime',
      })
      return nodesApi({ self: refused, nodes: [refused, pod] })
    })(),
    act: async (page) => {
      await page.getByText('Technical details').click()
    },
  },
  {
    id: 'insight-nodes-long',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: long names, big holdings, busy',
    since: '#402',
    note: 'Name column is 150px; long hostnames, detail ellipsis, "holds 1.3 TB", many running/awaiting.',
    api: nodesApi({
      self: node({ id: SELF, name: 'workstation-bazzite-living-room.home.arpa', is_self: true }),
      nodes: [
        node({ id: SELF, name: 'workstation-bazzite-living-room.home.arpa', is_self: true }),
        node({
          id: POD,
          name: 'kubernetes-pod-tracon-runner-7f9c6d5b8-x2kqz',
          harnesses: [harness('claude', '2.5.0-beta.20261001+build.4471', '2.5.0-beta.20261001+build.4471'), harness('opencode', '1.4.2', '1.4.2')],
          models: Array.from({ length: 17 }, (_, i) => ({ value: `m-${i}`, name: `Model ${i}` })),
        }),
        node({ id: DESK, name: '' }),
      ],
      data: data(1_304_000_000_000),
      queue: {
        waiting: Array.from({ length: 6 }, (_, i) => permission(`perm-${i}`, `s-${i}`, SELF)),
        reviews: [],
        promotions: [],
        running: Array.from({ length: 12 }, (_, i) => session(`s-${i}`, i < 8 ? SELF : POD)),
        ended: [],
      },
    }),
  },
  {
    id: 'insight-nodes-data-error',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: data inventory unavailable',
    since: '#402',
    note: 'The "holds" fragment is simply absent when /api/maintenance/data fails.',
    api: nodesApi({ data: err(500, 'database is locked') }),
  },
  {
    id: 'insight-nodes-waiting',
    area: 'insight',
    route: '/nodes',
    title: 'Nodes: no node answered yet',
    api: nodesApi({ self: hang, nodes: hang, mesh: hang }),
  },
  {
    id: 'insight-nodes-awake-held',
    area: 'insight',
    route: '/nodes',
    title: 'Rail: keeping the machine awake',
    since: '#379',
    note: 'Rail foot "keeping awake · <reason>" in the accent colour; check ellipsis on a long reason.',
    api: nodesApi({
      awake: { held: true, reason: '2 sessions working, 1 permission waiting', method: 'logind', error: null, last_suspend: null },
    }),
  },
  {
    id: 'insight-nodes-awake-error-slept',
    area: 'insight',
    route: '/nodes',
    title: 'Rail: cannot keep awake, and slept recently',
    since: '#379',
    note: 'Both warn lines in the rail foot together.',
    api: nodesApi({
      // asleep_ms is a duration, but the runner stamps every small *_ms as
      // an offset from now; a string body reaches call() unstamped.
      awake: () => ({
        status: 200,
        body: JSON.stringify({
          held: false,
          reason: null,
          method: 'logind',
          error: 'spawn systemd-inhibit: Permission denied (os error 13)',
          last_suspend: { woke_ms: Date.now() - 720_000, asleep_ms: 2_820_000 },
        }),
      }),
    }),
  },

  // ---- Memories
  {
    id: 'insight-memories-populated',
    area: 'insight',
    route: '/memories',
    title: 'Memories: a handful, every kind',
    note: 'Only active/promoted shown (three seeded non-live rows must not render); channel select present.',
    api: memoriesApi({ personal: typicalMemories, work: workMemories }),
  },
  {
    id: 'insight-memories-loading',
    area: 'insight',
    route: '/memories',
    title: 'Memories: loading',
    api: { '/api/memories': hang },
  },
  {
    id: 'insight-memories-empty',
    area: 'insight',
    route: '/memories',
    title: 'Memories: none on the channel',
    api: memoriesApi({}),
  },
  {
    id: 'insight-memories-error',
    area: 'insight',
    route: '/memories',
    title: 'Memories: list fails',
    note: 'Error banner and the empty line under it.',
    api: { '/api/memories': err(500, 'corpus is unavailable: database is locked') },
  },
  {
    id: 'insight-memories-long',
    area: 'insight',
    route: '/memories',
    title: 'Memories: long, multi-line and unbroken bodies',
    note: 'Wrapping of a long path and a 140-char hash, preserved newlines, long kind label.',
    api: memoriesApi({ personal: longMemories }),
  },
  {
    id: 'insight-memories-many',
    area: 'insight',
    route: '/memories',
    title: 'Memories: forty rows',
    api: memoriesApi({ personal: manyMemories }),
  },
  {
    id: 'insight-memories-single-channel',
    area: 'insight',
    route: '/memories',
    title: 'Memories: only one channel (no selector)',
    api: {
      ...memoriesApi({ personal: typicalMemories }),
      '/api/channels': [channelsWithDesk[0]],
    },
  },
  {
    id: 'insight-memories-channel-switch',
    area: 'insight',
    route: '/memories',
    title: 'Memories: switched to another channel',
    api: memoriesApi({ personal: typicalMemories, work: workMemories }),
    act: async (page) => {
      await page.locator('.bar select').selectOption('work')
    },
  },
  {
    id: 'insight-memories-edit',
    area: 'insight',
    route: '/memories',
    title: 'Memories: editing one in place',
    api: memoriesApi({ personal: typicalMemories }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Edit' }).nth(1).click()
    },
  },
  {
    id: 'insight-memories-edit-blank',
    area: 'insight',
    route: '/memories',
    title: 'Memories: editing, body cleared (Save disabled)',
    api: memoriesApi({ personal: typicalMemories }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Edit' }).first().click()
      await page.locator('textarea').fill('')
    },
  },
  {
    id: 'insight-memories-edit-error',
    area: 'insight',
    route: '/memories',
    title: 'Memories: save refused (memory gone)',
    note: 'Banner above the list while the editor stays open.',
    api: {
      ...memoriesApi({ personal: typicalMemories }),
      'PATCH /api/memories/*': err(404, 'no such memory'),
    },
    act: async (page) => {
      await page.getByRole('button', { name: 'Edit' }).nth(2).click()
      await page.locator('textarea').fill('The forge client must not retry a 422: the payload will not become valid.')
      await page.getByRole('button', { name: 'Save' }).click()
    },
  },
  {
    id: 'insight-memories-delete-confirm',
    area: 'insight',
    route: '/memories',
    title: 'Memories: confirming a delete',
    note: 'The confirm step replaces the row actions; deletes reach every node on the channel.',
    api: memoriesApi({ personal: typicalMemories }),
    act: async (page) => {
      await page.getByRole('button', { name: 'Delete' }).first().click()
    },
  },
  {
    id: 'insight-memories-delete-error',
    area: 'insight',
    route: '/memories',
    title: 'Memories: delete fails',
    api: {
      ...memoriesApi({ personal: typicalMemories }),
      'DELETE /api/memories/*': err(500, 'replicate tombstone: hub refused the change (channel key rotated)'),
    },
    act: async (page) => {
      await page.getByRole('button', { name: 'Delete' }).first().click()
      await page.locator('.act.confirm .btn').click()
    },
  },
  {
    id: 'insight-memories-deleted',
    area: 'insight',
    route: '/memories',
    title: 'Memories: one deleted, list reloaded',
    api: {
      ...memoriesApi({ personal: typicalMemories }),
      'DELETE /api/memories/*': { ok: true },
    },
    act: async (page) => {
      await page.route((url) => url.pathname === '/api/memories', (route) =>
        route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({ memories: typicalMemories.slice(1).map((m) => ({ ...m, updated_ms: Date.now() + m.updated_ms, created_ms: Date.now() + m.created_ms })) }),
        }),
      )
      await page.getByRole('button', { name: 'Delete' }).first().click()
      await page.locator('.act.confirm .btn').click()
    },
  },
]
