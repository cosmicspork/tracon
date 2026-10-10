// Home and the shell around it: the rail and its footer, the phone tabs, every
// banner App.svelte can raise, each card the home's lanes render, the
// first-run guide and setup checklist, readiness under the composer, plus the
// login gate, /nodes/enroll, the desktop app's setup page, and the two
// Settings cards (this device's push enrollment, hub summaries) the home
// links to.
//
// The baseline's ended sessions say `state: "ended"`, which is not a session
// state (types.ts `SessionState`); their rows lose the kind label and show an
// age instead of a duration. Every state here serves its own queue and
// session list with `closed`.

const SELF = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
const PEER = '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1'
const PEER2 = 'c7d1e4a09b3f6852d0e7a4c1b8f5e2d9a6c3b0f7e4d1a8c5b2f9e6d3a0c7b4e1'

const anthropic = (channels = ['personal', 'work'], o = {}) => ({
  name: 'anthropic',
  state: 'connected',
  kind: 'oauth',
  can_login: true,
  url: null,
  error: null,
  identity: 'op@example.com',
  expires_ms: 5400000,
  channels,
  updated_ms: -3600000,
  ...o,
})

const node = (o = {}) => ({
  id: SELF,
  name: 'laptop',
  state: 'ready',
  failed_check: null,
  failed_detail: null,
  harness: { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false },
  models: [
    { value: 'sonnet', name: 'Sonnet' },
    { value: 'opus', name: 'Opus' },
  ],
  checked_at_ms: -420000,
  is_self: true,
  reachable: true,
  last_seen_ms: null,
  loopback: true,
  default_channel: 'personal',
  application_version: '0.29.0',
  providers: [anthropic()],
  ...o,
})

const peer = (o = {}) =>
  node({
    id: PEER,
    name: 'work-pod',
    checked_at_ms: -60000,
    is_self: false,
    reachable: true,
    last_seen_ms: -22000,
    loopback: undefined,
    default_channel: undefined,
    providers: [anthropic(['work'], { expires_ms: 4100000, updated_ms: -86400000 })],
    ...o,
  })

const mesh = (hub = { state: 'connected' }, o = {}) => ({
  hub,
  hub_url: 'https://hub.example.net',
  node_id: SELF,
  fingerprint: 'SHA256:q3J8n0xZ5kTt1bYw2c9dVmR4hL7sPeAa6fGuKiNo0Ew',
  last_ok_ms: -4000,
  queued: 0,
  delivered_since_reconnect: 0,
  undecryptable: 0,
  held: 0,
  last_error: null,
  last_refusal: null,
  ...o,
})

const channel = (name, nodes, o = {}) => ({
  name,
  nodes,
  bindings: { phases: { plan: { model: 'sonnet' }, execute: { model: 'opus' } } },
  ceiling: { usage_today: 412000, ceiling: 4000000, state: 'under' },
  archived: null,
  ...o,
})

const CHANNELS = [
  channel('personal', [SELF, PEER]),
  channel('work', [PEER], {
    bindings: { phases: { review: { model: 'opus' } } },
    ceiling: { usage_today: 1660000, ceiling: 2000000, state: 'near' },
  }),
]

const PROVIDERS = [
  anthropic(),
  {
    name: 'openai',
    state: 'disconnected',
    kind: null,
    can_login: false,
    url: null,
    error: null,
    identity: null,
    expires_ms: null,
    channels: [],
    updated_ms: null,
  },
]

const session = (o) => ({
  node_id: SELF,
  channel: 'personal',
  work_item_id: null,
  repo_path: '/home/op/src/orbit',
  worktree_path: `/var/lib/tracon/worktrees/${o.id}`,
  harness_id: 'claude',
  harness_version: '2.5.0',
  harness_agent: 'claude-code',
  harness_found: '2.5.0',
  harness_protocol: '1',
  model: 'sonnet',
  phase: 'execute',
  policy_version: 12,
  review_id: null,
  budget_tokens: 2000000,
  tokens_used: 412000,
  cost_usd: 2.87,
  context_used: null,
  context_size: null,
  state: 'running',
  end_reason: null,
  last_error: null,
  turn_active: 0,
  draft: null,
  created_ms: -2700000,
  updated_ms: -140000,
  archived_ms: null,
  ...o,
})

const S_RUN = session({
  id: 's-run',
  work_item_id: 'wi-2',
  branch: 'feat/rate-limits',
  state: 'waiting_on_you',
  context_used: 84000,
  context_size: 200000,
})
const S_PLAN = session({
  id: 's-plan',
  node_id: PEER,
  channel: 'work',
  work_item_id: 'wi-6',
  repo_path: '/srv/repos/platform/orbit',
  branch: 'feat/queue-metrics',
  review_id: 'rev-1',
  tokens_used: 918000,
  cost_usd: 6.4,
  state: 'closed',
  end_reason: 'phase_done',
  created_ms: -10800000,
  updated_ms: -1560000,
})
const S_OLD = session({
  id: 's-old',
  work_item_id: 'wi-1',
  branch: 'feat/session-cookies',
  tokens_used: 1240000,
  cost_usd: 8.9,
  state: 'closed',
  end_reason: 'item_close',
  created_ms: -93600000,
  updated_ms: -90000000,
})

const queue = (o = {}) => ({ waiting: [], reviews: [], promotions: [], running: [], ended: [], ...o })
const sessionsOf = (q) => [...q.running, ...q.ended]

const permission = (o = {}) => ({
  id: 'perm-1',
  session_id: 's-run',
  node_id: SELF,
  title: 'Bash: just test',
  kind: 'execute',
  raw_input: JSON.stringify({ command: 'just test', cwd: '/work' }),
  options: JSON.stringify([
    { option_id: 'allow_once', name: 'Allow', kind: 'allow_once' },
    { option_id: 'reject_once', name: 'Deny', kind: 'reject_once' },
  ]),
  state: 'new',
  created_ms: -140000,
  expires_ms: 760000,
  intent: {
    channel: 'personal',
    phase: 'execute',
    branch: 'feat/rate-limits',
    work_item_id: 'wi-2',
    work_item_title: 'Rate-limit the public API per token',
    session_state: 'waiting_on_you',
  },
  ...o,
})

const CARD_OPTIONS = JSON.stringify([
  { option_id: 'allow_once', name: 'Allow once', kind: 'allow_once' },
  { option_id: 'reject_once', name: 'Reject', kind: 'reject_once' },
  { option_id: 'request_changes', name: 'Request changes', kind: 'request_changes' },
])
const EGRESS_OPTIONS = JSON.stringify([
  { option_id: 'allow_once', name: 'Allow once', kind: 'allow_once' },
  { option_id: 'allow_session', name: 'For this session', kind: 'allow_session' },
  { option_id: 'allow_repo', name: "Save to the repository's egress", kind: 'allow_repo' },
  { option_id: 'reject_once', name: 'Reject', kind: 'reject_once' },
])

const approval = (o = {}) =>
  permission({
    id: 'ap-7c21',
    title: 'pr_comment on op/orbit#212',
    kind: 'tool',
    options: CARD_OPTIONS,
    raw_input: JSON.stringify({
      tool: 'pr_comment',
      arguments: {
        repo: 'op/orbit',
        number: 212,
        body: 'The limiter now keys on the token id rather than the client IP, so two clients behind one NAT no longer share a bucket. Benchmarks are in the PR description.',
      },
      approval_id: 'ap-7c21',
      lane: null,
    }),
    created_ms: -300000,
    expires_ms: 3300000,
    ...o,
  })

const egress = (o = {}) =>
  permission({
    id: 'ap-egress-3',
    title: 'Reach crates.io',
    kind: 'tool',
    options: EGRESS_OPTIONS,
    raw_input: JSON.stringify({
      tool: 'request_egress',
      arguments: { host: 'index.crates.io', reason: 'cargo fetch for the new governor dependency' },
      approval_id: 'ap-egress-3',
      lane: null,
    }),
    created_ms: -60000,
    expires_ms: 840000,
    ...o,
  })

const review = (o = {}) => ({
  id: 'rev-1',
  session_id: 's-plan',
  lane: null,
  node_id: PEER,
  channel: 'work',
  kind: 'pr',
  title: 'Export queue metrics per channel',
  body: 'Adds the per-channel counters the dashboard reads.',
  edited_title: null,
  edited_body: null,
  provider: 'github',
  target: 'op/orbit',
  diff: '',
  files: JSON.stringify(['src/metrics.rs', 'src/gateway.rs']),
  head_sha: '8c4be91',
  base_ref: 'origin/main',
  added: 184,
  removed: 32,
  state: 'new',
  verdict_reason: null,
  publish_result: null,
  claimed_ms: null,
  created_ms: -1560000,
  checks_json: JSON.stringify([{ command: 'just check', ok: true, exit: 0, tail: 'ok', ms: 94000 }]),
  review_session_id: 's-review',
  ai_verdict_json: JSON.stringify({
    verdict: 'approve',
    summary: 'Counters match the ledger; no schema change.',
    findings: [],
    model: 'opus',
    session_id: 's-review',
    at_ms: 0,
  }),
  ...o,
})

const question = (o = {}) => ({
  id: 'q-1',
  session_id: 's-run',
  channel: 'personal',
  node_id: SELF,
  prompt: 'The v1 rate-limit route is still called by the mobile client. Keep it behind the new limiter, or remove it in this change?',
  choices_json: JSON.stringify(['Keep v1 behind the limiter', 'Remove v1 now']),
  request_key: null,
  state: 'unanswered',
  answer_json: null,
  created_ms: -420000,
  answered_ms: null,
  ...o,
})

const issue = (o = {}) => ({
  id: 'iss-4f2a',
  session_id: 's-run',
  channel: 'personal',
  title: 'review_status returns still_waiting after the review was approved',
  body: 'Calling review_status with an approved review id keeps answering still_waiting for ~40 s before reporting approved.\n\nExpected: the verdict as soon as it is recorded.',
  attachments_json: JSON.stringify([
    { name: 'review_status.log', content: '12:04:11 review_status r-91 -> still_waiting\n12:04:56 review_status r-91 -> approved' },
  ]),
  state: 'draft',
  published_url: null,
  published_number: null,
  publish_error: null,
  created_ms: -900000,
  approved_ms: null,
  decided_ms: null,
  discard_reason: null,
  node_id: SELF,
  ...o,
})

const promotion = (o = {}) => ({
  id: 'p-1',
  channel: 'personal',
  items_json: JSON.stringify([
    { memory_id: 'm-1', kind: 'fact', scope: 'repo', scope_ref: 'op/orbit', body: 'orbit runs its checks with `just check`.', confidence: 0.92, source_session: 's-old', source_node: SELF, created_ms: -90000000 },
    { memory_id: 'm-2', kind: 'lesson', scope: 'repo', scope_ref: 'op/orbit', body: 'Migrations must be reversible; CI runs down then up.', confidence: 0.81, source_session: 's-old', source_node: SELF, created_ms: -90000000 },
    { memory_id: 'm-3', kind: 'lesson', scope: 'channel', scope_ref: null, body: 'Prefer squash merges.', confidence: 0.7, source_session: 's-plan', source_node: PEER, created_ms: -10000000 },
  ]),
  state: 'open',
  verdicts_json: JSON.stringify({ 'm-1': 'promote' }),
  decided_by: null,
  decided_ms: null,
  site: SELF,
  hlc_ms: -3000000,
  created_ms: -3000000,
  ...o,
})

const pathReady = (purpose, missing = [], notes = []) => ({ purpose, ready: missing.length === 0, missing, notes })

/** Readiness answered for whatever repository the composer asks about. */
const readiness = (paths) => (req) => ({
  channel: req.query.channel,
  repo: req.query.repo,
  investigate: pathReady('investigate'),
  verify: pathReady('verify'),
  publish: pathReady('publish'),
  ...paths,
})

const BASE_QUEUE = queue({
  waiting: [permission()],
  reviews: [review()],
  running: [S_RUN],
  ended: [S_PLAN, S_OLD],
})

/** The baseline home, with session states the node actually stores. */
const base = (o = {}) => {
  const q = o['/api/queue'] ?? BASE_QUEUE
  return {
    '/api/node': node(),
    '/api/nodes': [node(), peer()],
    '/api/mesh': mesh(),
    '/api/channels': CHANNELS,
    '/api/providers': PROVIDERS,
    '/api/queue': q,
    '/api/sessions': q.running ? sessionsOf(q) : sessionsOf(BASE_QUEUE),
    '/api/awake': { held: false, reason: null, method: 'logind', error: null, last_suspend: null },
    '/api/readiness': readiness({}),
    // Nothing in the checkout the node would refuse to prepare.
    '/api/preparation': (req) => ({ repo: req.query.repo, image: 'localhost/tracon-claude:2.5.0', image_source: 'harness image', devcontainer_image: null, lockfiles: [], install: null, prepare: [], egress: [], incompatible: [], ready: true }),
    ...o,
  }
}

const never = () => new Promise(() => {})

// `asleep_ms` is a duration, but the runner stamps every small *_ms value as
// an offset from now; a string body is sent as-is, so stamp `woke_ms` here.
const awake = ({ last_suspend, ...rest }) => () =>
  JSON.stringify({
    ...rest,
    last_suspend: last_suspend && { woke_ms: Date.now() - last_suspend.woke_ago, asleep_ms: last_suspend.asleep_ms },
  })

// A stream the page can be driven through: `window.__stream.emit(frames)`
// dispatches frames as the node's SSE would. `fail` drops the connection
// after it opened, as a node that went away does.
const stream = ({ fail = false } = {}) => `(() => {
  const stamp = (v) => Array.isArray(v) ? v.map(stamp) : v && typeof v === 'object'
    ? Object.fromEntries(Object.entries(v).map(([k, x]) => [k, k.endsWith('_ms') && typeof x === 'number' && Math.abs(x) < 1e12 ? Date.now() + x : stamp(x)]))
    : v;
  class Stream extends EventTarget {
    constructor(url) {
      super();
      this.url = url; this.readyState = 0; this.onopen = null; this.onerror = null; this.onmessage = null;
      window.__stream = this;
      setTimeout(() => {
        this.readyState = 1;
        this.onopen && this.onopen(new Event('open'));
        if (${fail}) setTimeout(() => { this.readyState = 0; this.onerror && this.onerror(new Event('error')); }, 150);
      }, 30);
    }
    emit(frames) { for (const f of frames) this.dispatchEvent(new MessageEvent(f.type, { data: JSON.stringify(stamp(f)) })); }
    close() { this.readyState = 2; }
  }
  window.EventSource = Stream;
})()`

const emit = (frames) => async (page) => {
  await page.evaluate((f) => window.__stream.emit(f), frames)
}

/** Stubs the desktop app's IPC: every setup command answers with `status`. */
const tauri = (status, { reject = null, hang = false } = {}) => `(() => {
  globalThis.isTauri = true;
  window.__TAURI_INTERNALS__ = {
    invoke: (cmd) => ${hang ? 'new Promise(() => {})' : reject ? `Promise.reject(${JSON.stringify(reject)})` : `Promise.resolve(${JSON.stringify(status)})`},
    transformCallback: () => 0,
    unregisterCallback: () => {},
    convertFileSrc: (p) => p,
  };
})()`

const setupStatus = (o = {}) => ({
  platform: 'linux',
  owner: 'service',
  node_version: '0.29.0',
  sidecar_version: '0.29.0',
  cli_version: '0.29.0',
  cli_path: '/home/op/.local/bin/tracon',
  path_hint: null,
  service_installed: true,
  service_running: true,
  service_failing: false,
  service_error: null,
  podman: '/usr/bin/podman',
  machine: null,
  machine_error: null,
  ...o,
})

// The rail is sticky; a full-page shot taken after an act scrolled the page
// leaves it floating mid-page. Scroll back first.
const top = (act) => async (page, ctx) => {
  await act(page, ctx)
  await page.evaluate(() => window.scrollTo(0, 0))
}

const clickText = (text) =>
  top(async (page) => {
    await page.getByText(text, { exact: false }).first().click()
  })

/** What Settings fetches beside the cards these states look at. */
const settings = (o = {}) =>
  base({
    '/api/config': { node_name: 'laptop', running: { harness_id: 'claude', harness_version: '2.5.0', node_name: 'laptop' }, external: { enabled: false } },
    '/api/maintenance/data': { total_bytes: 3_412_000_000, kinds: [] },
    '/api/authority/grants': { policy: { version: 12, rules: [], trusted: true }, grants: [] },
    '/api/admin/access': { authenticated: true, token_configured: true, local: true },
    '/api/admin/mesh': {
      local: true,
      inventory_source: 'hub',
      mesh: null,
      members: [],
      channels: [],
      capabilities: {
        invite: true,
        remove_member: true,
        share_existing_channel_with_hub: true,
        edit_member_channels: { available: false, reason: 'membership is changed by re-inviting the node' },
      },
    },
    ...o,
  })

const LONG_BRANCH = 'feat/replace-the-per-ip-token-bucket-with-a-per-credential-sliding-window-limiter-and-backfill-metrics'
const LONG_REPO = '/home/op/src/clients/acme-corporation/internal-platform-services/monorepo-with-a-very-long-name'

// ---------------------------------------------------------------------------

export default [
  // --- Home: typical, empty, loading, failures ------------------------------
  {
    id: 'home-populated',
    area: 'home',
    route: '/',
    title: 'Home, typical day',
    note: 'Baseline home with corrected session states: landed rows should read "Reviewed"/"Ended · item closed" and "ran …".',
    api: base(),
  },
  {
    id: 'home-rail-collapsed',
    area: 'home',
    route: '/',
    title: 'Collapsed rail with waiting badge',
    note: 'The narrow rail: brand mark as the expand button, the waiting count tucked on the Home icon, footer hidden.',
    init: () => localStorage.setItem('tracon-rail', 'narrow'),
    api: base(),
    sizes: ['desktop'],
  },
  {
    id: 'home-empty-first-task',
    area: 'home',
    route: '/',
    title: 'Nothing yet: first task guide',
    note: 'No sessions anywhere: FirstTaskGuide above the composer, local and peer targets.',
    api: base({ '/api/queue': queue() }),
  },
  {
    id: 'home-first-task-peer-selected',
    area: 'home',
    route: '/',
    title: 'First task guide, a peer chosen, details open',
    note: 'Choosing a peer target: the route section, the work channel ceiling line, the expanded budgets/approvals details.',
    api: base({ '/api/queue': queue() }),
    act: async (page) => {
      await page.locator('.guide button', { hasText: 'work-pod' }).first().click()
      await page.locator('.guide .route details summary').click()
    },
  },
  {
    id: 'home-first-task-no-local',
    area: 'home',
    route: '/',
    title: 'First task guide, this node cannot run',
    note: 'This node refused isolation, so only the peer is offered; "Run here" explains why with a Prepare link.',
    api: base({
      '/api/queue': queue(),
      '/api/nodes': [
        node({ state: 'refused', failed_check: 'egress', failed_detail: 'gateway answered on 10.89.0.1:3128 but allowed a direct connection to 1.1.1.1:443' }),
        peer(),
      ],
    }),
  },
  {
    id: 'home-setup-fresh',
    area: 'home',
    route: '/',
    title: 'Setup checklist, fresh node',
    note: 'No provider, no channel, no peer: SetupCard with every step outstanding. Check the divider above "Use an existing setup" (uses --line).',
    api: base({
      '/api/queue': queue(),
      '/api/nodes': [node({ models: [], providers: [] })],
      '/api/channels': [],
      '/api/providers': [],
      '/api/mesh': mesh({ state: 'disabled' }, { hub_url: null, fingerprint: null, last_ok_ms: null }),
    }),
  },
  {
    id: 'home-setup-partial',
    area: 'home',
    route: '/',
    title: 'Setup checklist, two steps left',
    note: 'Provider connected and a channel exists, but this node is not a member and no model is offered to it. Expand the optional device setup.',
    api: base({
      '/api/queue': queue(),
      '/api/nodes': [node(), peer({ reachable: false, last_seen_ms: -7200000 })],
      '/api/channels': [channel('work', [PEER])],
    }),
    act: clickText('Optional device setup'),
  },
  {
    id: 'home-loading',
    area: 'home',
    route: '/',
    title: 'First paint, snapshots still loading',
    note: 'What the home shows before the node answers: loading, not the setup checklist.',
    api: {
      '/api/nodes': never,
      '/api/mesh': never,
      '/api/channels': never,
      '/api/queue': never,
      '/api/sessions': never,
      '/api/providers': never,
      '/api/awake': never,
    },
  },
  {
    id: 'home-queue-error',
    area: 'home',
    route: '/',
    title: 'Queue snapshot failed (500)',
    note: 'GET /api/queue answers 500 and every other snapshot succeeds: a banner says what is waiting could not be loaded, with a Retry.',
    api: base({
      '/api/queue': { status: 500, body: { error: { code: 500, message: 'database is locked' } } },
      '/api/sessions': sessionsOf(BASE_QUEUE),
    }),
  },
  {
    id: 'home-channels-error',
    area: 'home',
    route: '/',
    title: 'Channels snapshot failed (500)',
    note: 'Without the channel list the home cannot tell a configured node from a new one: it says it could not load, with a retry, and no setup checklist.',
    api: base({
      '/api/channels': { status: 500, body: { error: { code: 500, message: 'database is locked' } } },
    }),
  },

  // --- Shell banners and footer -------------------------------------------
  {
    id: 'home-awake-held',
    area: 'home',
    route: '/',
    title: 'Footer: keeping the machine awake',
    since: '#379',
    note: 'Rail footer line in the accent colour: "keeping awake · 1 session working, 1 permission waiting". Check truncation at 172px.',
    api: base({
      '/api/awake': { held: true, reason: '1 session working, 1 permission waiting', method: 'logind', error: null, last_suspend: null },
    }),
    sizes: ['desktop'],
  },
  {
    id: 'home-awake-error-slept',
    area: 'home',
    route: '/',
    title: 'Footer: cannot keep awake, and slept recently',
    since: '#379',
    note: 'Both warning lines: the hold failed, and the machine slept 47 min twelve minutes ago.',
    api: base({
      '/api/awake': awake({
        held: false,
        reason: null,
        method: 'logind',
        error: 'could not start systemd-inhibit: No such file or directory (os error 2)',
        last_suspend: { woke_ago: 720000, asleep_ms: 2820000 },
      }),
    }),
    sizes: ['desktop'],
  },
  {
    id: 'home-awake-collapsed',
    area: 'home',
    route: '/',
    title: 'Footer awake state with the rail collapsed',
    since: '#379',
    note: 'Collapsed rail hides the footer, so the awake/slept lines vanish: is there any other cue?',
    init: () => localStorage.setItem('tracon-rail', 'narrow'),
    api: base({
      '/api/awake': awake({ held: true, reason: '2 sessions working', method: 'logind', error: null, last_suspend: { woke_ago: 300000, asleep_ms: 600000 } }),
    }),
    sizes: ['desktop'],
  },
  {
    id: 'home-connection-lost',
    area: 'home',
    route: '/',
    title: 'Banner: connection lost',
    note: 'Stream dropped after it opened; footer reads "offline".',
    init: stream({ fail: true }),
    api: base(),
  },
  {
    id: 'home-node-refused',
    area: 'home',
    route: '/',
    title: 'Banner: node failed its isolation check, details open',
    note: 'Refused banner with the technical details expanded (check, what it saw, remedy). Long unbroken detail text.',
    api: base({
      '/api/nodes': [
        node({
          state: 'refused',
          failed_check: 'network_isolated',
          failed_detail:
            'harness container could reach http://169.254.169.254/latest/meta-data/ directly; expected the internal network tracon-boundary-net-9f31c6a870d24b5e to have no route except via the gateway',
        }),
        peer(),
      ],
    }),
    act: clickText('Technical details'),
  },
  {
    id: 'home-node-cannot-run',
    area: 'home',
    route: '/',
    title: 'Banner: the serving node cannot run tasks',
    note: 'Its only harness reports another version than it pins: the banner names the gap and links to setup. The peer can still run, so the composer stays.',
    api: base({
      '/api/nodes': [node({ harness: { id: 'claude', pinned: '2.5.0', found: '2.3.1', mismatch: true } }), peer()],
    }),
  },
  {
    id: 'home-more',
    area: 'home',
    route: '/more',
    title: 'More: the destinations the phone bar has no room for',
    note: 'Memories, Evidence, Usage and Settings; the More tab is lit.',
    api: base(),
    sizes: ['phone'],
  },
  {
    id: 'home-stale-version',
    area: 'home',
    route: '/',
    title: 'Banner: the node was upgraded under this page',
    note: 'Node reports 0.30.0 while the page is 0.29.0: the reload banner.',
    api: base({ '/api/nodes': [node({ application_version: '0.30.0' }), peer()] }),
  },
  {
    id: 'home-hub-down',
    area: 'home',
    route: '/',
    title: 'Banner: hub unavailable, peer work held',
    note: 'Hub unreachable: dim banner, footer "hub down 18m", the peer review moves to "Outside this node" with its reason; held chips dimmed.',
    api: base({ '/api/mesh': mesh({ state: 'unreachable', since_ms: -1080000 }, { last_error: 'connect: connection refused', queued: 3 }) }),
  },
  {
    id: 'home-hub-reconnected',
    area: 'home',
    route: '/',
    title: 'Banner: hub reconnected',
    note: 'The hub came back while the page was open: green banner with the queued count.',
    init: stream(),
    api: base({ '/api/mesh': mesh({ state: 'unreachable', since_ms: -600000 }, { queued: 3 }) }),
    act: emit([{ type: 'mesh', ...mesh({ state: 'connected' }, { queued: 3, delivered_since_reconnect: 3 }) }]),
  },
  {
    id: 'home-banners-stacked',
    area: 'home',
    route: '/',
    title: 'Every banner at once',
    note: 'Connection lost + refused + stale version + hub down stacked above the composer; especially on phone.',
    init: stream({ fail: true }),
    api: base({
      '/api/nodes': [
        node({ state: 'refused', failed_check: 'runtime', failed_detail: 'podman: cannot connect to the Podman socket', application_version: '0.30.0' }),
        peer(),
      ],
      '/api/mesh': mesh({ state: 'unreachable', since_ms: -5400000 }),
    }),
  },

  // --- Lanes and cards ------------------------------------------------------
  {
    id: 'home-decisions-all-kinds',
    area: 'home',
    route: '/',
    title: 'Waiting on you: every card kind',
    note: 'Question (choices), question (free text), permission, brokered approval with an editable prose field, egress request, review, narrative report, issue draft, memory batch. Compare the question/issue cards (bordered, unthemed) with the row cards.',
    api: base({
      '/api/queue': queue({
        waiting: [permission(), approval(), egress()],
        reviews: [
          review(),
          review({
            id: 'rev-2',
            node_id: SELF,
            channel: 'personal',
            session_id: 's-run',
            title: 'Investigation: why the nightly export doubles rows',
            kind: 'report',
            files: '[]',
            added: 0,
            removed: 0,
            checks_json: null,
            ai_verdict_json: null,
            review_session_id: null,
            created_ms: -600000,
          }),
        ],
        promotions: [promotion()],
        running: [S_RUN],
        ended: [S_PLAN, S_OLD],
      }),
      '/api/operator/questions': {
        questions: [
          question(),
          question({ id: 'q-2', choices_json: '[]', prompt: 'Which staging database should the migration dry-run against?', created_ms: -200000 }),
        ],
      },
      '/api/operator/issues': { issues: [issue()] },
    }),
  },
  {
    id: 'home-gated-approvals',
    area: 'home',
    route: '/',
    title: 'Waiting on you: gated tools titled in words',
    note: 'pr_merge, run_rerun, service_start, issue_transition and repo_setup_propose cards carry the titles mcp::summarize gives them, not their arguments as JSON. The transition names the status it leads to.',
    api: base({
      '/api/queue': queue({
        waiting: [
          ['ap-merge', 'pr_merge', 'pr_merge op/orbit#212: squash at 3e1f0a2', { repo: 'op/orbit', number: 212, head_sha: '3e1f0a2c9b8d7e6f5a4b', method: 'squash', operation_id: 'op-1' }],
          ['ap-rerun', 'run_rerun', 'run_rerun op/orbit: failed jobs of run 11893472205', { repo: 'op/orbit', run_id: 11893472205 }],
          ['ap-service', 'service_start', 'service_start postgres', { name: 'postgres', wait_secs: 30 }],
          ['ap-transition', 'issue_transition', 'issue_transition WRK-1874: move to In Review', { key: 'WRK-1874', transition_id: '31', operation_id: 'op-2' }],
          ['ap-propose', 'repo_setup_propose', 'repo_setup_propose /var/home/op/src/orbit: Tried the draft twice in a fresh container.', { repo: '/var/home/op/src/orbit', checks: ['cargo nextest run'], why: 'Tried the draft twice in a fresh container.' }],
        ].map(([id, tool, title, args], i) =>
          approval({
            id,
            title,
            raw_input: JSON.stringify({ tool, arguments: args, approval_id: id, lane: null }),
            created_ms: -60000 * (i + 1),
          }),
        ),
        running: [S_RUN],
        ended: [S_PLAN, S_OLD],
      }),
    }),
  },
  {
    id: 'home-agent-and-external-lanes',
    area: 'home',
    route: '/',
    title: 'With the agent / Outside this node',
    note: 'Off-decision lanes with their reasons: a review being revised, an expired request, one publishing, an uncertain issue publication with a failure, a peer-held request.',
    api: base({
      '/api/nodes': [node(), peer({ reachable: false, last_seen_ms: -1500000 })],
      '/api/queue': queue({
        waiting: [
          permission({ id: 'perm-exp', title: 'Bash: cargo build --release', raw_input: JSON.stringify({ command: 'cargo build --release' }), created_ms: -1500000, expires_ms: -300000 }),
          permission({ id: 'perm-peer', node_id: PEER, session_id: 's-plan', title: 'Edit: src/gateway.rs', kind: 'edit', raw_input: JSON.stringify({ path: 'src/gateway.rs' }), intent: { channel: 'work', phase: 'execute', branch: 'feat/queue-metrics', work_item_id: 'wi-6', work_item_title: 'Queue metrics per channel', session_state: 'running' } }),
        ],
        reviews: [
          review({ id: 'rev-3', node_id: SELF, channel: 'personal', state: 'revising', title: 'Rate-limit the public API per token', verdict_reason: 'Add a test for the burst case', created_ms: -2400000 }),
          review({ id: 'rev-4', node_id: SELF, channel: 'personal', state: 'publishing', title: 'Drop the unused v0 health endpoint', created_ms: -480000 }),
          review({ id: 'rev-5', created_ms: -3600000 }),
        ],
        running: [S_RUN],
        ended: [S_PLAN, S_OLD],
      }),
      '/api/operator/issues': {
        issues: [
          issue({ id: 'iss-9', state: 'uncertain', publish_error: 'POST /repos/cosmicspork/tracon/issues: timed out after 30 s; the issue may exist', created_ms: -5400000 }),
          issue({ id: 'iss-10', state: 'publishing', title: 'pr_status shows a stale head after a force-push', created_ms: -60000 }),
        ],
      },
    }),
  },
  {
    id: 'home-permission-request-open',
    area: 'home',
    route: '/',
    title: 'Permission card with the full request open',
    note: 'Expanded "Full request" with a long multi-line command; the summary line ellipsises the command.',
    api: base({
      '/api/queue': queue({
        waiting: [
          permission({
            title: 'Bash: cargo nextest run',
            raw_input: JSON.stringify({
              command:
                'cargo nextest run --workspace --all-features --no-fail-fast -E "test(/rate_limit::/) | test(/governor::/)" -- --test-threads=4 2>&1 | tee /tmp/nextest-rate-limits-$(date +%s).log',
              cwd: '/work/orbit',
              timeout: 600000,
            }),
          }),
        ],
        running: [S_RUN],
      }),
    }),
    act: clickText('Full request'),
  },
  {
    id: 'home-permission-answer-error',
    area: 'home',
    route: '/',
    title: 'Allow refused by the node (409)',
    note: 'The node rejects the answer: the error is appended to the card summary line.',
    api: base({
      '/api/queue': queue({ waiting: [permission()], running: [S_RUN] }),
      'POST /api/permissions/*/answer': { status: 409, body: { error: { code: 409, message: 'this request was already answered on another device' } } },
      'POST /api/permissions/perm-1/answer': { status: 409, body: { error: { code: 409, message: 'this request was already answered on another device' } } },
    }),
    act: async (page) => {
      await page.locator('button', { hasText: /^Allow$/ }).first().click()
    },
  },
  {
    id: 'home-question-answer-error',
    area: 'home',
    route: '/',
    title: 'Operator question: answer failed',
    note: 'Typed a free-text answer, the node answered 500: error line under the card (uses --red, which is not defined).',
    api: base({
      '/api/queue': queue({ running: [S_RUN] }),
      '/api/operator/questions': {
        questions: [question({ choices_json: '[]', prompt: 'Which staging database should the migration dry-run against?\nThe two candidates are orbit-staging-eu and orbit-staging-us.' })],
      },
      'POST /api/operator/questions/*/answer': { status: 500, body: { error: { code: 500, message: 'the session that asked has ended' } } },
    }),
    act: async (page) => {
      await page.getByPlaceholder('Free-text answer').fill('orbit-staging-eu')
      await page.getByRole('button', { name: 'Answer' }).click()
    },
  },
  {
    id: 'home-issue-discard-open',
    area: 'home',
    route: '/',
    title: 'Issue draft: outgoing body and discard form open',
    note: 'Issue card with "Exact outgoing issue body" and attachments expanded and the discard reason form showing.',
    api: base({
      '/api/queue': queue({ running: [S_RUN] }),
      '/api/operator/issues': { issues: [issue()] },
    }),
    act: top(async (page) => {
      await page.getByText('Exact outgoing issue body').click()
      await page.getByText('1 inspectable attachment').click()
      await page.getByRole('button', { name: 'Discard…' }).click()
    }),
  },
  {
    id: 'home-reviews-variants',
    area: 'home',
    route: '/',
    title: 'Review cards: verdicts, checks, claimed, GitLab',
    note: 'AI verdict "changes suggested", still reviewing, no checks, claimed, an MR, and an acknowledged-report label.',
    api: base({
      '/api/queue': queue({
        reviews: [
          review({ id: 'rev-a', node_id: SELF, channel: 'personal', ai_verdict_json: JSON.stringify({ verdict: 'request_changes', summary: 'Burst handling untested.', findings: [], model: 'opus', session_id: 's-review', at_ms: 0 }), title: 'Rate-limit the public API per token', created_ms: -3000000 }),
          review({ id: 'rev-b', node_id: SELF, channel: 'personal', ai_verdict_json: null, title: 'Bump axum to 0.9', files: JSON.stringify(['Cargo.toml', 'Cargo.lock']), added: 412, removed: 398, created_ms: -2000000 }),
          review({ id: 'rev-c', node_id: SELF, channel: 'personal', checks_json: null, ai_verdict_json: null, review_session_id: null, claimed_ms: -60000, state: 'claimed', title: 'Docs: explain the per-token limiter', files: JSON.stringify(['docs/limits.md']), added: 38, removed: 2, created_ms: -1000000 }),
          review({ id: 'rev-d', provider: 'gitlab', target: 'platform/orbit', title: 'Export queue metrics per channel', created_ms: -500000 }),
        ],
        running: [S_RUN],
      }),
    }),
  },
  {
    id: 'home-long-content',
    area: 'home',
    route: '/',
    title: 'Overflow: long titles, branches, repos, many rows',
    note: 'Long unbroken branch and repo names, a 160-char review title, a long permission title, 7 running sessions and 6 landed.',
    api: (() => {
      const running = Array.from({ length: 7 }, (_, i) =>
        session({
          id: `s-r${i}`,
          branch: i === 0 ? LONG_BRANCH : `feat/task-${i}-${'x'.repeat(i * 6)}`,
          repo_path: i % 2 ? LONG_REPO : '/home/op/src/orbit',
          state: ['running', 'running', 'waiting_on_check', 'paused', 'starting', 'suspended', 'waiting_on_you'][i],
          turn_active: i === 0 ? 1 : 0,
          tokens_used: 150000 * (i + 1),
          model: i === 3 ? 'anthropic/claude-opus-4-20250514-with-a-long-suffix' : 'sonnet',
          updated_ms: -60000 * (i + 1),
        }),
      )
      const ended = Array.from({ length: 6 }, (_, i) =>
        session({
          id: `s-e${i}`,
          branch: i === 0 ? `${LONG_BRANCH}-again` : `fix/landed-${i}`,
          repo_path: i === 1 ? LONG_REPO : '/home/op/src/orbit',
          state: 'closed',
          end_reason: 'phase_done',
          phase: i % 2 ? 'plan' : 'execute',
          created_ms: -86400000 - i * 3600000,
          updated_ms: -80000000 - i * 3600000,
          tokens_used: 1999000,
        }),
      )
      const q = queue({
        waiting: [
          permission({
            title: 'Bash: ./scripts/regenerate-fixtures-for-every-supported-database-engine-and-locale-combination.sh --all --verbose',
            raw_input: JSON.stringify({ command: './scripts/regenerate-fixtures-for-every-supported-database-engine-and-locale-combination.sh --all --verbose' }),
            intent: { channel: 'personal', phase: 'execute', branch: LONG_BRANCH, work_item_id: 'wi-2', work_item_title: 'Replace the per-IP token bucket with a per-credential sliding window limiter, and backfill the metrics', session_state: 'running' },
          }),
        ],
        reviews: [
          review({
            id: 'rev-long',
            node_id: SELF,
            channel: 'personal',
            title: 'Replace the per-IP token bucket with a per-credential sliding-window limiter, backfill the per-channel usage metrics, and document the migration path for v1 callers',
            files: JSON.stringify(Array.from({ length: 48 }, (_, i) => `src/f${i}.rs`)),
            added: 12840,
            removed: 9321,
          }),
        ],
        running,
        ended,
      })
      return base({ '/api/queue': q, '/api/sessions': sessionsOf(q) })
    })(),
  },
  {
    id: 'home-session-end-reasons',
    area: 'home',
    route: '/',
    title: 'Landed rows: every way a session ends',
    since: '#375',
    note: 'Ended · node restarted (#375), provider exhausted, continued, failed with a humanised error, killed on budget, external harness detached. Also a peer-owned running row whose owner went quiet.',
    api: (() => {
      const q = queue({
        running: [
          session({ id: 's-peer', node_id: PEER, channel: 'work', branch: 'feat/queue-metrics', state: 'running', turn_active: 1, updated_ms: -1500000 }),
          session({ id: 's-ext', harness_id: 'external', harness_version: '', harness_agent: 'claude-code', harness_session_id: '2f0c9a1e-77b4-4d0a-9a51-0c3c0f6b7e21', repo_path: '', branch: 'repo-orbit', state: 'running', tokens_used: 0, budget_tokens: 0 }),
        ],
        ended: [
          session({ id: 's-nr', branch: 'feat/rate-limits', state: 'closed', end_reason: 'node_restart', last_error: 'node restarted while the session was live', created_ms: -7200000, updated_ms: -3600000 }),
          session({ id: 's-px', branch: 'fix/flaky-retry', state: 'closed', end_reason: 'provider_exhausted', created_ms: -9000000, updated_ms: -7000000 }),
          session({ id: 's-ct', branch: 'feat/rate-limits', state: 'closed', end_reason: 'continued', created_ms: -12000000, updated_ms: -7200000 }),
          session({ id: 's-fl', branch: 'chore/bump-deps', state: 'failed', last_error: 'rpc: rpc error -32603: Internal error (Unknown tool mcp__tracon__pipeline_wait)', created_ms: -20000000, updated_ms: -19000000 }),
          session({ id: 's-kb', branch: 'feat/full-text-search', state: 'killed_budget', tokens_used: 2004000, created_ms: -30000000, updated_ms: -25000000 }),
          session({ id: 's-dt', harness_id: 'external', harness_version: '', harness_agent: 'opencode', repo_path: '', branch: 'lane-docs', state: 'closed', end_reason: 'detached', tokens_used: 0, budget_tokens: 0, created_ms: -40000000, updated_ms: -36000000 }),
        ],
      })
      return base({ '/api/queue': q, '/api/sessions': sessionsOf(q), '/api/nodes': [node(), peer({ reachable: false, last_seen_ms: -1500000 })] })
    })(),
  },
  {
    id: 'home-notification-deliveries',
    area: 'home',
    route: '/',
    title: 'Notification delivery attempts, expanded',
    note: 'The deliveries <details> under the lanes, opened: device ids are cut to 8 characters.',
    api: base({
      '/api/operator/notifications': {
        notifications: [
          { notification_id: '7d3f0a2c-6c1e-4b8e-9a52-1f0e4c7b2d91', expires_ms: 3000000, attempts: [{ device_id: 'b81c2e4f-0d7a-4c51-8e3b-9f2a6d1c0e57', outcome: 'service accepted (201)' }, { device_id: `node:${PEER}`, outcome: 'peer acknowledged delivery request' }] },
          { notification_id: 'e19b7c40-2a5d-4f3e-b6c8-70d1a9e2f4b3', expires_ms: 2400000, attempts: [] },
          { notification_id: '0c4a9e7f-81b2-4d6c-a3f5-2e7b0d9c1f68', expires_ms: 1200000, attempts: [{ device_id: `node:${PEER}`, outcome: 'refused: mesh is disabled' }] },
        ],
        receipt: 'device push-service attempts and peer acknowledgements only; human receipt is unknown',
      },
    }),
    act: clickText('Notification delivery attempts'),
  },

  // --- Your own agents (external lanes) -------------------------------------
  {
    id: 'home-external-agents',
    area: 'home',
    route: '/',
    title: 'Your own agents: lanes and a stopped channel',
    note: 'Running, exited and unknown-liveness lanes, one with requests waiting, an unlabelled lane, and the stopped-broker banner.',
    api: base({
      '/api/external': {
        enabled: true,
        channels: ['personal', 'work'],
        lanes: [
          { channel: 'personal', lane: 'tracon-spa-audit', last_ms: -40000, calls: 128, pending: 2, running: 1 },
          { channel: 'personal', lane: 'orbit-rate-limits', last_ms: -1800000, calls: 1, pending: 0, running: 0 },
          { channel: 'personal', lane: null, last_ms: -7200000, calls: 14, pending: 0, running: null },
        ],
        stopped: ['work'],
      },
    }),
  },
  {
    id: 'home-external-lane-log',
    area: 'home',
    route: '/',
    title: 'Your own agents: lane log open, pipeline followed',
    since: '#393',
    note: 'An expanded lane log with a followed pipeline: "following", job results, pipeline failed; plus approvals and a review submitted.',
    api: base({
      '/api/external': {
        enabled: true,
        channels: ['personal'],
        lanes: [{ channel: 'personal', lane: 'orbit-rate-limits', last_ms: -30000, calls: 23, pending: 1, running: 1 }],
        stopped: [],
      },
      '/api/external/personal/events': {
        events: [
          { seq: 1, channel: 'personal', node_id: SELF, lane: 'orbit-rate-limits', kind: 'tool_call', ref_id: null, payload: { title: 'submit_review' }, at_ms: -1500000 },
          { seq: 2, channel: 'personal', node_id: SELF, lane: 'orbit-rate-limits', kind: 'tool_result', ref_id: null, payload: { title: 'submit_review', status: 'approved' }, at_ms: -900000 },
          { seq: 3, channel: 'personal', node_id: SELF, lane: 'orbit-rate-limits', kind: 'pipeline_follow', ref_id: 'pf-1', payload: { project: 'op/orbit', pipeline_id: 18342, url: 'https://github.com/op/orbit/actions/runs/18342', started_by: 'run_rerun', changes: ['following'] }, at_ms: -600000 },
          { seq: 4, channel: 'personal', node_id: SELF, lane: 'orbit-rate-limits', kind: 'pipeline_follow', ref_id: 'pf-1', payload: { project: 'op/orbit', pipeline_id: 18342, url: 'https://github.com/op/orbit/actions/runs/18342', status: 'running', changes: ['`lint` passed', '`unit` passed'] }, at_ms: -400000 },
          { seq: 5, channel: 'personal', node_id: SELF, lane: 'orbit-rate-limits', kind: 'pipeline_follow', ref_id: 'pf-1', payload: { project: 'op/orbit', pipeline_id: 18342, url: 'https://github.com/op/orbit/actions/runs/18342', status: 'failed', changes: ['`e2e (chromium)` failed', '`e2e (webkit)` canceled', 'pipeline failed'] }, at_ms: -200000 },
          { seq: 6, channel: 'personal', node_id: SELF, lane: 'orbit-rate-limits', kind: 'approval_requested', ref_id: 'ap-7c21', payload: { title: 'pr_comment on op/orbit#212' }, at_ms: -60000 },
          { seq: 7, channel: 'personal', node_id: SELF, lane: 'orbit-rate-limits', kind: 'work_closed', ref_id: null, payload: { title: 'Rate-limit the public API per token', state: 'closed' }, at_ms: -30000 },
        ],
      },
    }),
    act: clickText('orbit-rate-limits'),
  },

  // --- Readiness as a banner above the composer ------------------------------
  {
    id: 'home-readiness-ready',
    area: 'home',
    route: '/',
    title: 'Readiness: all three paths ready',
    since: '#387',
    note: 'A ready repository shows nothing: no banner, no line under the composer.',
    api: base(),
    act: async (page) => page.waitForTimeout(600),
  },
  {
    id: 'home-readiness-gaps',
    area: 'home',
    route: '/',
    title: 'Readiness: verify and publish lacking',
    since: '#387',
    note: 'A banner above the composer: no checks configured (verify) and a GitHub credential not bound (publish), each named once with where to close it; notes are not gaps and are left out.',
    api: base({
      '/api/readiness': readiness({
        verify: pathReady(
          'verify',
          [{ key: 'checks', message: 'no required checks are configured for this repository, so nothing the node runs can verify a result; add `checks` to its [[repo]] entry' }],
          [{ key: 'image', message: "checks run in the node's harness image (ghcr.io/cosmicspork/tracon-harness:0.29.0); the repository names no toolchain of its own" }],
        ),
        publish: pathReady(
          'publish',
          [
            { key: 'checks', message: 'no required checks are configured for this repository, so nothing the node runs can verify a result; add `checks` to its [[repo]] entry' },
            { key: 'credential', message: 'GitHub credential `gh`: not bound to channel personal on this node' },
          ],
          [{ key: 'brief', message: 'the work item has no brief, so its review shows no requirements to judge against' }],
        ),
      }),
    }),
    act: async (page) => page.locator('.banner.ready').waitFor(),
  },
  {
    id: 'home-readiness-not-a-repo',
    area: 'home',
    route: '/',
    title: 'Readiness: nothing ready (path is not a checkout)',
    since: '#387',
    note: 'A crit banner: the repository path does not exist on this node, a long path that must wrap; no link for gaps fixed in the repository itself.',
    api: base({
      '/api/readiness': (req) => {
        const gap = { key: 'repo', message: `${req.query.repo} is not a directory on this node` }
        return {
          channel: req.query.channel,
          repo: req.query.repo,
          investigate: pathReady('investigate', [gap]),
          verify: pathReady('verify', [gap, { key: 'checks', message: 'no required checks are configured for this repository, so nothing the node runs can verify a result; add `checks` to its [[repo]] entry' }]),
          publish: pathReady('publish', [gap, { key: 'remote', message: 'the repository has no `origin` remote to publish to' }]),
        }
      },
      '/api/queue': queue({ ended: [session({ id: 's-gone', repo_path: LONG_REPO, branch: 'feat/old', state: 'closed', end_reason: 'item_close', created_ms: -100000, updated_ms: -50000 })] }),
      '/api/sessions': [session({ id: 's-gone', repo_path: LONG_REPO, branch: 'feat/old', state: 'closed', end_reason: 'item_close', created_ms: -100000, updated_ms: -50000 })],
    }),
    act: async (page) => page.locator('.banner.ready').waitFor(),
  },
  {
    id: 'home-start-error',
    area: 'home',
    route: '/',
    title: 'Composer: the node refused to start the session',
    note: 'Start pressed and the node answered 409 at its ceiling: the "could not start" banner inside the composer.',
    api: base({
      'POST /api/sessions': { status: 409, body: { error: { code: 409, message: 'channel personal is at its daily ceiling (4.0M tokens); new sessions are refused until 00:00 UTC' } } },
    }),
    act: async (page) => {
      await page.locator('textarea').first().fill('Add a burst test for the per-token limiter')
      await page.getByRole('button', { name: 'Start session' }).click()
    },
  },

  // --- Login gate -------------------------------------------------------------
  {
    id: 'home-login',
    area: 'home',
    route: '/',
    title: 'Login gate',
    note: 'Every snapshot answers 401: the operator token form replaces the shell.',
    api: {
      '/api/*': { status: 401, body: { error: { code: 401, message: 'log in' } } },
      '/api/operator/*': { status: 401, body: { error: { code: 401, message: 'log in' } } },
    },
  },
  {
    id: 'home-login-rejected',
    area: 'home',
    route: '/',
    title: 'Login gate, token not accepted',
    note: 'A token typed and refused: the error line under the field.',
    api: {
      '/api/*': { status: 401, body: { error: { code: 401, message: 'log in' } } },
      '/api/operator/*': { status: 401, body: { error: { code: 401, message: 'log in' } } },
      'POST /api/login': { status: 401, body: { error: { code: 401, message: 'token not accepted' } } },
    },
    act: async (page) => {
      await page.getByPlaceholder('trc1.…').fill('trc1.9f31c6a870d24b5e.wrongwrongwrong')
      await page.getByRole('button', { name: 'Log in' }).click()
    },
  },
  {
    id: 'home-login-checking',
    area: 'home',
    route: '/',
    title: 'Login gate, checking the token',
    note: 'The exchange in flight: button reads "Checking…", input disabled.',
    api: {
      '/api/*': { status: 401, body: { error: { code: 401, message: 'log in' } } },
      '/api/operator/*': { status: 401, body: { error: { code: 401, message: 'log in' } } },
      'POST /api/login': never,
    },
    act: async (page) => {
      await page.getByPlaceholder('trc1.…').fill('trc1.9f31c6a870d24b5e.0123456789abcdef')
      await page.getByRole('button', { name: 'Log in' }).click()
    },
  },

  // --- Enroll -----------------------------------------------------------------
  {
    id: 'home-enroll-local',
    area: 'home',
    route: '/nodes/enroll',
    title: 'Enroll, on the serving node',
    note: 'Hub configured, reached over loopback: the three explanatory sections and the Mesh administration button.',
    api: base(),
  },
  {
    id: 'home-enroll-remote',
    area: 'home',
    route: '/nodes/enroll',
    title: 'Enroll, reached remotely',
    note: 'Not loopback: the extra note under the button.',
    api: base({ '/api/nodes': [node({ loopback: false }), peer()] }),
  },
  {
    id: 'home-enroll-no-hub',
    area: 'home',
    route: '/nodes/enroll',
    title: 'Enroll without a hub',
    note: 'No hub configured: the crit banner instead of the sections.',
    api: base({ '/api/mesh': mesh({ state: 'disabled' }, { hub_url: null, fingerprint: null, last_ok_ms: null }) }),
  },

  // --- Desktop setup (/setup.html) ------------------------------------------
  {
    id: 'home-setup-desktop-fresh',
    area: 'home',
    route: '/setup.html',
    title: 'Desktop setup, nothing installed (Linux)',
    note: 'No Podman, no service, no CLI: install link, disabled Install button (blocked by Podman).',
    init: tauri(setupStatus({ owner: 'none', node_version: null, cli_version: null, service_installed: false, service_running: false, podman: null })),
  },
  {
    id: 'home-setup-desktop-macos-machine',
    area: 'home',
    route: '/setup.html',
    title: 'Desktop setup, macOS Podman machine undetectable',
    note: 'Podman found, machine detection failed with an error, and the CLI is an older version.',
    init: tauri(
      setupStatus({
        platform: 'macos',
        owner: 'none',
        node_version: null,
        service_installed: false,
        service_running: false,
        podman: '/opt/homebrew/bin/podman',
        machine: 'error',
        machine_error: 'podman machine list: exit status 125: Error: cannot connect to Podman socket at /var/folders/7x/q1w2e3r4t5y6u7i8o9p0/T/podman/podman-machine-default-api.sock',
        cli_version: '0.27.2',
        cli_path: '/Users/op/.local/bin/tracon',
      }),
    ),
  },
  {
    id: 'home-setup-desktop-failing',
    area: 'home',
    route: '/setup.html',
    title: 'Desktop setup, service keeps exiting',
    note: 'Installed but crash-looping: its last words in a reason block, Restart and Reinstall buttons, PATH hint for the CLI.',
    init: tauri(
      setupStatus({
        owner: 'none',
        service_running: false,
        service_failing: true,
        service_error: 'Error: open /home/op/.local/state/tracon/node.db: database is locked (another tracon node is running with the same TRACON_STATE_DIR?)',
        path_hint: 'export PATH="$HOME/.local/bin:$PATH"',
      }),
    ),
  },
  {
    id: 'home-setup-desktop-ready',
    area: 'home',
    route: '/setup.html',
    title: 'Desktop setup, everything done',
    note: 'All steps checked, Restart node offered, Open tracon.',
    init: tauri(setupStatus()),
  },
  {
    id: 'home-setup-desktop-foreign',
    area: 'home',
    route: '/setup.html',
    title: 'Desktop setup, a node started outside the app',
    note: 'A foreign node answers: service step explains, no install button, Open tracon still offered.',
    init: tauri(setupStatus({ owner: 'foreign', service_installed: false, service_running: false })),
  },
  {
    id: 'home-setup-desktop-loading',
    area: 'home',
    route: '/setup.html',
    title: 'Desktop setup, looking at the machine',
    note: 'The status call has not answered yet.',
    init: tauri(null, { hang: true }),
    sizes: ['desktop'],
  },
  {
    id: 'home-setup-desktop-error',
    area: 'home',
    route: '/setup.html',
    title: 'Desktop setup, status command failed',
    note: 'The IPC call rejects: only the error line shows, with no steps and no way to retry but waiting.',
    init: tauri(null, { reject: 'desktop_setup_status: failed to read ~/.config/systemd/user/tracon.service: Permission denied (os error 13)' }),
  },

  // --- Settings cards the home links to -----------------------------------
  {
    id: 'home-push-devices',
    area: 'home',
    route: '/settings#access',
    title: 'This device and registered devices',
    note: 'Push toggle, and four registered devices: this browser, a local one, a failing phone, a long user agent.',
    api: settings({
      '/api/push/subscriptions': {
        devices: [
          { id: 'd-1', user_agent: 'Chrome 142 on Linux', created_ms: -86400000, last_ok_ms: -120000, fail_count: 0, local: true, mine: true },
          { id: 'd-2', user_agent: 'Safari on iPhone (Home Screen)', created_ms: -604800000, last_ok_ms: -3600000, fail_count: 0, local: false, mine: false },
          { id: 'd-3', user_agent: 'Firefox 145 on Android', created_ms: -1209600000, last_ok_ms: null, fail_count: 7, local: false, mine: false },
          { id: 'd-4', user_agent: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36 Edg/142.0.0.0', created_ms: -2592000000, last_ok_ms: -2000000000, fail_count: 1, local: false, mine: false },
        ],
      },
    }),
  },
  {
    id: 'home-push-denied',
    area: 'home',
    route: '/settings#access',
    title: 'Push enrollment: notifications blocked',
    since: '#403',
    note: 'Turning push on where the site is blocked: the stage message in the crit colour.',
    init: () => {
      Notification.requestPermission = () => Promise.resolve('denied')
    },
    api: settings(),
    act: async (page) => {
      await page.getByText(/Push from .* to this device/).click()
      await page.waitForSelector('.note')
    },
  },
  {
    id: 'home-push-service-refused',
    area: 'home',
    route: '/settings#access',
    title: "Push enrollment: the browser's push service refused",
    since: '#403',
    note: 'Permission granted, the subscribe call rejected by the push service: the message names that stage and quotes the browser.',
    init: () => {
      Notification.requestPermission = () => Promise.resolve('granted')
      PushManager.prototype.getSubscription = () => Promise.resolve(null)
      PushManager.prototype.subscribe = () => Promise.reject(new DOMException('Registration failed - push service not available', 'AbortError'))
    },
    api: settings(),
    act: async (page) => {
      await page.getByText(/Push from .* to this device/).click()
      await page.waitForSelector('.note')
    },
  },
  {
    id: 'home-push-node-refused',
    area: 'home',
    route: '/settings#access',
    title: 'Push enrollment: the node did not record the device',
    since: '#403',
    note: 'Subscribed in the browser, then POST /api/push/subscriptions answered 500: "its storage failed: …".',
    init: () => {
      Notification.requestPermission = () => Promise.resolve('granted')
      const sub = {
        endpoint: 'https://fcm.googleapis.com/fcm/send/dXJ0aGVyZS1pcy1ub3RoaW5nLWhlcmU',
        toJSON: () => ({ endpoint: 'https://fcm.googleapis.com/fcm/send/dXJ0aGVyZS1pcy1ub3RoaW5nLWhlcmU', keys: { p256dh: 'BPx', auth: 'aa' } }),
        unsubscribe: () => Promise.resolve(true),
      }
      PushManager.prototype.getSubscription = () => Promise.resolve(null)
      PushManager.prototype.subscribe = () => Promise.resolve(sub)
    },
    api: settings({
      'POST /api/push/subscriptions': { status: 500, body: { error: { code: 500, message: 'database is locked' } } },
    }),
    act: async (page) => {
      await page.getByText(/Push from .* to this device/).click()
      await page.waitForSelector('.note')
    },
  },
  {
    id: 'home-hub-rollups',
    area: 'home',
    route: '/settings#mesh',
    title: 'Hub summaries: one complete, one partial',
    note: 'HubRollups card at the bottom of Mesh: personal current and complete, work with a missing and a stale member.',
    api: settings({
      '/api/mesh/rollups': (req) => {
        const summary = (nodeId, o = {}) => ({
          node_id: nodeId,
          seq: 412,
          captured_ms: -90000,
          received_ms: -85000,
          complete: true,
          freshness: 'current',
          session_counts: { running: 1, closed: 14 },
          queued_permissions: 1,
          queued_reviews: 1,
          work_items: 9,
          documents: 37,
          memories: 120,
          ...o,
        })
        return req.query.channel === 'work'
          ? {
              channel: 'work',
              state: 'partial',
              summaries: [summary(PEER, { freshness: 'stale', captured_ms: -5400000, complete: false, session_counts: {} })],
              coverage: { expected_nodes: [PEER, PEER2], missing_nodes: [PEER2], stale_nodes: [PEER], partial_nodes: [PEER], current_complete: false },
            }
          : {
              channel: 'personal',
              state: 'current_complete',
              summaries: [summary(SELF), summary(PEER, { session_counts: { running: 0 }, queued_permissions: 0, work_items: 2, documents: 4 })],
              coverage: { expected_nodes: [SELF, PEER], missing_nodes: [], stale_nodes: [], partial_nodes: [], current_complete: true },
            }
      },
    }),
  },
  {
    id: 'home-hub-rollups-unavailable',
    area: 'home',
    route: '/settings#mesh',
    title: 'Hub summaries: hub refused',
    note: 'The rollup request fails for a reason other than an unshared channel: the unavailable line.',
    api: settings({
      '/api/mesh/rollups': { status: 502, body: { error: { code: 502, message: 'hub answered 503 Service Unavailable' } } },
    }),
  },
]
