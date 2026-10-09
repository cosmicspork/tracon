// Settings (/settings and its #sections): every card in its populated, empty,
// error and editing states. The baseline api.json lacks most of what Settings
// reads (/api/config, /api/authority/grants, the admin routes, the manifest,
// the repository table, the data inventory) and its /api/mesh and /api/nodes
// rows are older shapes, so every state here starts from BASE below.

const SELF = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
const PEER = '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1'
const GONE = 'c27d90e1b4a6f3582d0c9e7b1a4f6d3c8e2b5a9f0d7c4e1b6a3f8d2c5e9b0a7d'

const MODELS = [
  { value: 'anthropic/sonnet', name: 'Claude Sonnet', harnesses: ['claude', 'opencode'] },
  { value: 'anthropic/opus', name: 'Claude Opus', harnesses: ['claude', 'opencode'] },
  { value: 'anthropic/haiku', name: 'Claude Haiku', harnesses: ['claude', 'opencode'] },
  { value: 'openrouter/qwen3-coder', name: 'Qwen3 Coder (OpenRouter)', harnesses: ['opencode'] },
  { value: 'openrouter/deepseek-v3', name: 'DeepSeek V3 (OpenRouter)', harnesses: ['opencode'] },
]

const P_ANTHROPIC = {
  name: 'anthropic',
  state: 'connected',
  kind: 'oauth',
  can_login: true,
  url: null,
  error: null,
  identity: 'operator@example.com',
  expires_ms: 5400000,
  channels: ['personal', 'work'],
  updated_ms: -3600000,
}
const P_CODEX = {
  name: 'openai-codex',
  state: 'pending',
  kind: null,
  can_login: true,
  url: 'https://auth.openai.com/codex/device',
  completion: 'device_code',
  completion_note: 'The code expires in 15 minutes.',
  device_code: 'WDJB-MJHT',
  error: null,
  identity: null,
  expires_ms: null,
  channels: ['personal'],
  updated_ms: -60000,
}
const P_OPENROUTER = {
  name: 'openrouter',
  state: 'connected',
  kind: 'api_key',
  can_login: false,
  url: null,
  error: null,
  identity: null,
  expires_ms: null,
  channels: ['personal'],
  updated_ms: -86400000 * 3,
}
const P_OPENAI_FAILED = {
  name: 'openai',
  state: 'failed',
  kind: 'api_key',
  can_login: false,
  url: null,
  error: 'upstream refused the key: 401 invalid_api_key',
  identity: null,
  expires_ms: null,
  channels: ['work'],
  updated_ms: -7200000,
}
const PROVIDERS = [P_ANTHROPIC, P_CODEX, P_OPENROUTER, P_OPENAI_FAILED]

const HARNESSES = [
  { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false, default: true, image: 'localhost/tracon-claude:2.5.0', image_state: 'current' },
  { id: 'opencode', pinned: '1.4.2', found: '1.4.2', mismatch: false, default: false, image: 'localhost/tracon-opencode:1.4.2', image_state: 'stale' },
]

const selfNode = (over = {}) => ({
  id: SELF,
  name: 'laptop',
  state: 'ready',
  failed_check: null,
  failed_detail: null,
  harness: { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false },
  harnesses: HARNESSES,
  models: MODELS,
  checked_at_ms: -420000,
  is_self: true,
  reachable: true,
  last_seen_ms: null,
  providers: PROVIDERS,
  loopback: true,
  default_channel: 'personal',
  application_version: '0.29.0',
  ...over,
})
const peerNode = (over = {}) => ({
  id: PEER,
  name: 'work-pod',
  state: 'ready',
  failed_check: null,
  failed_detail: null,
  harness: { id: 'claude', pinned: '2.5.0', found: '2.5.0', mismatch: false },
  models: MODELS.slice(0, 2),
  checked_at_ms: -60000,
  is_self: false,
  reachable: true,
  last_seen_ms: -22000,
  providers: [{ ...P_ANTHROPIC, channels: ['work'], expires_ms: 4100000 }],
  application_version: '0.29.0',
  ...over,
})
const goneNode = (over = {}) => ({
  ...peerNode(),
  id: GONE,
  name: 'build-box',
  reachable: false,
  last_seen_ms: -86400000 * 2,
  application_version: '0.27.1',
  providers: [],
  ...over,
})
const NODES = [selfNode(), peerNode(), goneNode()]

const MESH = {
  hub: { state: 'connected' },
  hub_url: 'https://hub.example.net',
  node_id: SELF,
  fingerprint: 'SHA256:q3x9Lw0b8Yk2mH7tR1cV5nP4sJ6dA0fE',
  last_ok_ms: -12000,
  queued: 0,
  delivered_since_reconnect: 184,
  undecryptable: 0,
  held: 0,
  last_error: null,
  last_refusal: null,
}
const MESH_OFF = { ...MESH, hub: { state: 'disabled' }, hub_url: null, fingerprint: null, last_ok_ms: null, delivered_since_reconnect: 0 }

const ceiling = (usage, cap, state, unmetered = 0) => ({ usage_today: usage, ceiling: cap, state, unmetered_turns: unmetered })
const CHANNELS = [
  {
    name: 'personal',
    nodes: [SELF, PEER],
    bindings: {
      phases: { plan: { model: 'anthropic/opus' }, execute: { model: 'anthropic/sonnet' } },
      exhaustion: { policy: 'fallback_then_wait', fallback: 'openrouter/qwen3-coder' },
    },
    ceiling: ceiling(412000, 4000000, 'under'),
    archived: null,
  },
  {
    name: 'work',
    nodes: [SELF, PEER, GONE],
    bindings: { phases: { plan: { model: 'anthropic/opus' } }, notify: { enabled: false } },
    ceiling: ceiling(1660000, 2000000, 'near', 3),
    archived: null,
  },
  {
    name: 'client-hdr',
    nodes: [SELF],
    bindings: { phases: { execute: { model: 'anthropic/haiku-legacy' } }, exhaustion: { policy: 'fallback' } },
    ceiling: ceiling(1000000, 1000000, 'at'),
    archived: null,
  },
  {
    name: 'scratch',
    nodes: [SELF],
    bindings: {},
    ceiling: ceiling(0, null, 'none'),
    archived: -86400000 * 9,
  },
]

const declared = (id, name, context, output, reasoning = true, attachment = false) => ({ id, name, context, output, reasoning, attachment })
const CONFIG = {
  node_name: 'laptop',
  harness: { id: 'claude', version: '2.5.0', tools: [] },
  session: { budget_tokens: 2000000, permission_timeout_secs: 900, approval_expiry_secs: 86400, default_channel: 'personal' },
  review: { max_diff_lines: 4000, max_files: 120 },
  gateway: { allow_hosts: [] },
  publish: { gh: '', glab: '', git: '' },
  boundary: { podman: '' },
  external: { enabled: true },
  launch: { plugins: [] },
  providers: {
    anthropic: {
      shape: 'anthropic',
      upstream: 'https://api.anthropic.com',
      credential: 'anthropic',
      login: 'claude',
      models: [declared('sonnet', 'Claude Sonnet', 200000, 64000), declared('opus', 'Claude Opus', 200000, 32000), declared('haiku', 'Claude Haiku', 200000, 8192, false)],
    },
    'openai-codex': { shape: 'openai', upstream: 'https://chatgpt.com/backend-api/codex', credential: 'openai-codex', login: 'codex', models: [] },
    openrouter: {
      shape: 'openai',
      upstream: 'https://openrouter.ai/api/v1',
      credential: 'openrouter',
      login: null,
      models: [
        declared('qwen/qwen3-coder', 'Qwen3 Coder', 262144, 65536),
        declared('deepseek/deepseek-chat-v3', 'DeepSeek V3', 163840, 16384, false),
        declared('moonshotai/kimi-k2', 'Kimi K2', 131072, 16384),
        declared('z-ai/glm-4.6', 'GLM 4.6', 202752, 65536),
        declared('mistralai/devstral-medium', 'Devstral Medium', 131072, 32768, false, true),
      ],
    },
    openai: { shape: 'openai', upstream: 'https://api.openai.com/v1', credential: 'openai', login: null, models: [declared('gpt-5', 'GPT-5', 400000, 128000, true, true)] },
  },
  supervision: { checks: ['just check'], timeout_secs: 900, dependency_inputs: [], max_snapshot_bytes: 52428800 },
  readonly: { hub_url: 'https://hub.example.net', runtime: 'podman', config_path: '/home/op/.config/tracon/node.toml' },
  running: { harness_id: 'claude', harness_version: '2.5.0', node_name: 'laptop' },
}

const CREDENTIALS = {
  credentials: [
    { name: 'gh', kind: 'env', provider: null, channels: ['personal', 'work'], nodes: [PEER], identity: 'op-bot', expires_ms: null, env_keys: ['GH_TOKEN'] },
    { name: 'anthropic', kind: 'oauth', provider: 'anthropic', channels: ['personal', 'work'], nodes: [PEER], identity: 'operator@example.com', expires_ms: 5400000, env_keys: [] },
    { name: 'openrouter', kind: 'api_key', provider: 'openrouter', channels: ['personal'], nodes: [], identity: null, expires_ms: null, env_keys: ['API_KEY'] },
    { name: 'jira', kind: 'env', provider: null, channels: ['work'], nodes: [PEER], identity: null, expires_ms: null, env_keys: ['JIRA_URL', 'JIRA_EMAIL', 'JIRA_TOKEN'] },
    { name: 'sentry-readonly', kind: 'env', provider: null, channels: [], nodes: [], identity: null, expires_ms: null, env_keys: ['SENTRY_AUTH_TOKEN'] },
  ],
}

const rule = (id, verdict, reason, kinds, matches = []) => ({ id, verdict, reason, kinds, channels: [], matches, args: {} })
const RULES = [
  rule('read-anything', 'allow', 'Reading the workspace is the job.', ['read', 'glob', 'grep']),
  rule('edit-workspace', 'allow', 'Edits stay in the isolated worktree until reviewed.', ['edit', 'write']),
  rule('shell-ask', 'ask', 'A shell command can reach beyond the workspace.', ['bash']),
  rule('merge-ask', 'ask', 'Merging deploys on some repositories.', ['merge']),
  rule('force-push-deny', 'deny', 'History on a shared branch is never rewritten.', ['bash'], ['git push --force*', 'git push -f*']),
]
const GRANTS = [
  {
    id: 'g-1',
    action: 'merge',
    verdict: 'allow',
    target: 'github:op/orbit:pr:412',
    channel: 'personal',
    session_id: null,
    revision: '8d3f0a1c9e2b7d4f6a5c3e1b0d9f8a7c6e5d4b3a',
    expires_ms: 86400000,
    revoked_ms: null,
    reason: 'Release PR approved in chat; CI green on this head.',
    created_ms: -3600000,
  },
  {
    id: 'g-2',
    action: 'terminal',
    verdict: 'allow',
    target: 'terminal:s-run:/workspaces/s-run/orbit',
    channel: 'personal',
    session_id: 's-run',
    revision: null,
    expires_ms: null,
    revoked_ms: null,
    reason: 'Debug the flaky integration test interactively.',
    created_ms: -1800000,
  },
  {
    id: 'g-3',
    action: 'publish',
    verdict: 'deny',
    target: 'gitlab:platform/infrastructure/terraform-modules-shared-networking:mr:1187',
    channel: 'work',
    session_id: null,
    revision: null,
    expires_ms: null,
    revoked_ms: null,
    reason: 'Never publish to the shared networking modules from an agent session; changes there go through the platform team.',
    created_ms: -86400000 * 4,
  },
  {
    id: 'g-4',
    action: 'deploy',
    verdict: 'ask',
    target: 'gitlab:op/site:environment:staging',
    channel: 'work',
    session_id: null,
    revision: null,
    expires_ms: null,
    revoked_ms: -86400000,
    reason: 'Temporary staging access during the migration.',
    created_ms: -86400000 * 6,
  },
]
const AUTHORITY = { policy: { version: 12, rules: RULES, trusted: true }, grants: GRANTS }

const ACCESS = { authenticated: true, token_configured: true, local: true }

const compat = (over = {}) => ({
  runtime: 'compatible',
  pinned: '2.5.0',
  found: '2.5.0',
  harnesses: [
    { id: 'claude', default: true, runtime: 'compatible', pinned: '2.5.0', found: '2.5.0', image: null, image_state: 'current' },
    { id: 'opencode', default: false, runtime: 'compatible', pinned: '1.4.2', found: '1.4.2', image: null, image_state: 'stale' },
  ],
  checked_at_ms: -60000,
  models: { state: 'offered', offered: 5 },
  application: { state: 'compatible', local: '0.29.0', peer: '0.29.0' },
  wire: { state: 'compatible', local: 7, peer: 7 },
  policy: {
    identity: { state: 'compatible', local: 'ed25519:7Hq…c2Lw', peer: 'ed25519:7Hq…c2Lw' },
    bundle: { state: 'compatible', local: 'a1f9c04e77b2d3', peer: 'a1f9c04e77b2d3' },
    receipt: { state: 'supported', advertised: true, upgrade_needed: false },
  },
  ...over,
})
const ADMIN_MESH = {
  local: true,
  inventory_source: 'Membership as this node last read it from the hub directory, 40 s ago.',
  mesh: {},
  members: [
    {
      id: SELF,
      name: 'laptop',
      self: true,
      reachable: true,
      last_seen_ms: null,
      channels: ['@mesh', 'personal', 'work', 'client-hdr'],
      compatibility: compat({
        application: { state: 'local', local: '0.29.0', peer: null },
        wire: { state: 'local', local: 7, peer: null },
        policy: {
          identity: { state: 'local', local: 'ed25519:7Hq…c2Lw', peer: null },
          bundle: { state: 'local', local: 'a1f9c04e77b2d3', peer: null },
          receipt: { state: 'supported', advertised: true, upgrade_needed: false },
        },
      }),
    },
    { id: PEER, name: 'work-pod', self: false, reachable: true, last_seen_ms: -22000, channels: ['@mesh', 'personal', 'work'], compatibility: compat() },
    {
      id: GONE,
      name: 'build-box',
      self: false,
      reachable: false,
      last_seen_ms: -86400000 * 2,
      channels: ['@mesh', 'work'],
      compatibility: compat({
        runtime: 'mismatch',
        found: '2.3.1',
        harnesses: undefined,
        models: { state: 'unknown', offered: null },
        application: { state: 'mismatch', local: '0.29.0', peer: '0.27.1', upgrade_needed: true },
        wire: { state: 'mismatch', local: 7, peer: 6, upgrade_needed: true },
        policy: {
          identity: { state: 'compatible', local: 'ed25519:7Hq…c2Lw', peer: 'ed25519:7Hq…c2Lw' },
          bundle: { state: 'mismatch', local: 'a1f9c04e77b2d3', peer: '0be4d81c55a907' },
          receipt: { state: 'unknown_or_legacy', advertised: null, upgrade_needed: true },
        },
      }),
    },
  ],
  channels: [
    { name: 'personal', nodes: [SELF, PEER] },
    { name: 'work', nodes: [SELF, PEER, GONE] },
    { name: 'client-hdr', nodes: [SELF] },
  ],
  capabilities: {
    invite: true,
    remove_member: true,
    share_existing_channel_with_hub: true,
    edit_member_channels: { available: false, reason: 'A member’s channels change by inviting it again with the channels it should hold; keys already handed over are not taken back.' },
  },
}
const rollup = (channel, state, summaries, missing = [], stale = []) => ({
  channel,
  state,
  summaries,
  coverage: { expected_nodes: [SELF, PEER], missing_nodes: missing, stale_nodes: stale, partial_nodes: [], current_complete: state === 'current_complete' },
})
const sum = (node_id, freshness, running, waiting, work, docs, captured) => ({
  node_id,
  seq: 88,
  captured_ms: captured,
  received_ms: captured + 900,
  complete: true,
  freshness,
  session_counts: { running },
  queued_permissions: waiting,
  queued_reviews: 1,
  work_items: work,
  documents: docs,
  memories: 31,
})
const ROLLUPS = (req) => {
  const ch = req.query.channel
  if (ch === 'personal') return rollup('personal', 'current_complete', [sum(SELF, 'current', 2, 1, 14, 63, -40000), sum(PEER, 'current', 0, 0, 3, 63, -55000)])
  if (ch === 'work') return rollup('work', 'partial', [sum(PEER, 'stale', 1, 3, 22, 18, -3600000)], [GONE], [PEER])
  return { status: 403, body: { error: { code: 403, message: 'this channel has not been shared a key with the hub' } } }
}

const bundle = (version, sha) => ({
  toml: `version = ${version}\n\n[[rule]]\nid = "read-anything"\nverdict = "allow"\nkinds = ["read", "glob", "grep"]\nreason = "Reading the workspace is the job."\n\n[[rule]]\nid = "shell-ask"\nverdict = "ask"\nkinds = ["bash"]\nreason = "A shell command can reach beyond the workspace."\n\n[[rule]]\nid = "force-push-deny"\nverdict = "deny"\nkinds = ["bash"]\nmatches = ["git push --force*", "git push -f*"]\nreason = "History on a shared branch is never rewritten."\n`,
  signature: 'MEUCIQDx3…',
  trust_identity: 'ed25519:7HqP4mZ0bN9xVv2Kc8sTq1yWf6eR3uLa5jDg0oIhc2Lw',
  bundle_sha256: sha,
  policy: { version, rules: RULES, trusted: true },
})
const POLICY = {
  installed: bundle(12, 'a1f9c04e77b2d3e8c5b6f0a9d2e1c4b7a8f3d6e9c0b5a2f1e4d7c8b9a6f3e0d1'),
  installed_error: null,
  running: { version: 12, rule_count: 5, trusted: true },
  trust_identity: 'ed25519:7HqP4mZ0bN9xVv2Kc8sTq1yWf6eR3uLa5jDg0oIhc2Lw',
  installation: { bundle_sha256: 'a1f9c04e77b2d3e8c5b6f0a9d2e1c4b7a8f3d6e9c0b5a2f1e4d7c8b9a6f3e0d1', source_node: SELF, rollout_id: 'ro-12', applied_ms: -86400000 * 2 },
  rollouts: [
    {
      id: 'ro-12',
      bundle_sha256: 'a1f9c04e77b2d3e8c5b6f0a9d2e1c4b7a8f3d6e9c0b5a2f1e4d7c8b9a6f3e0d1',
      policy_version: 12,
      source_node: SELF,
      created_ms: -86400000 * 2,
      updated_ms: -86400000 * 2 + 60000,
      targets: [
        { node_id: PEER, status: 'applied', confirmation: 'applied', compatibility: 'receipt_capable', detail: null, attempts: 1, last_sent_ms: -86400000 * 2, acknowledged_ms: -86400000 * 2 + 4000 },
        { node_id: GONE, status: 'offline', confirmation: 'unconfirmed', compatibility: 'unknown_or_legacy', detail: 'node has not been seen since the rollout was created; it is sent again when it returns', attempts: 3, last_sent_ms: -86400000, acknowledged_ms: null },
      ],
    },
    {
      id: 'ro-11',
      bundle_sha256: '0be4d81c55a9073f2e6d1c8b4a7f0e3d9c2b5a8f1e4d7c0b3a6f9e2d5c8b1a4f',
      policy_version: 11,
      source_node: SELF,
      created_ms: -86400000 * 12,
      updated_ms: -86400000 * 12,
      targets: [{ node_id: PEER, status: 'rejected', confirmation: 'rejected', compatibility: 'receipt_capable', detail: 'signature verified, but the trust identity differs from the one this node pinned', attempts: 1, last_sent_ms: -86400000 * 12, acknowledged_ms: -86400000 * 12 + 2000 }],
    },
  ],
  signing_key_present: true,
}

const cap = (available, reason, recovery) => ({ available, reason, recovery })
const MAINTENANCE = {
  local: true,
  active_sessions: 2,
  service: {
    platform: 'linux',
    supervisor: 'systemd --user',
    container: 'podman 5.6.1 · rootless',
    unit_path: '/home/op/.config/systemd/user/tracon.service',
    unit_installed: true,
    state: { state: 'running', detail: 'active (running) since 3 h ago' },
    install: cap(true, 'The unit file can be written and the user manager reloaded.', 'Reinstalling rewrites the fixed unit; node.toml and state are untouched.'),
    restart: cap(false, '2 sessions are active; a restart would end them as node_restart.', 'Wait for them to finish or stop them, then restart.'),
    uninstall: cap(true, 'The unit is installed by tracon and can be removed.', 'Run `tracon service install` to bring it back.'),
  },
  boundary: selfNode(),
  mesh: {},
}

const holding = (kind, label, unit, count, bytes, del, propagation) => ({ kind, label, unit, count, bytes, delete: del, propagation })
const RUNTIME = { path: '/settings#maintenance', label: 'Maintenance, under Runtime storage, once the session is archived' }
const DATA = {
  database_bytes: 412_334_080,
  total_bytes: 6_948_120_576,
  holdings: [
    holding('sessions', 'Sessions', 'session', 1284, 38_221_504, null, 'Nothing deletes a session yet: its record is what its reviews, approvals and usage point at. Archiving one ends it and frees its workspace.'),
    holding('events', 'Session logs', 'event', 912_406, 301_884_416, null, 'Nothing deletes a log entry yet; a log is kept whole or not at all.'),
    holding('evidence', 'Candidates and evidence', 'candidate', 342, 41_009_152, null, 'Nothing deletes evidence yet: a verdict is only as good as what it was given, and a review may already be published.'),
    holding('documents', 'Documents', 'document', 63, 4_812_800, { path: '/docs', label: 'Documents, one at a time' }, 'A delete replicates to every node on the document’s channel and to the hub. Each keeps a tombstone, the row with its content cleared, so the delete wins over an older copy arriving later.'),
    holding('memories', 'Memories', 'memory', 1, 812, { path: '/memories', label: 'Memories, one at a time' }, 'A delete replicates to every node on the memory’s channel and to the hub, each keeping a tombstone, and drops it from recall.'),
    holding('work', 'Work items', 'work item', 118, 902_144, { path: '/work', label: 'Work, one item at a time' }, 'A delete replicates to every node on the item’s channel and to the hub, each keeping a tombstone. Sessions that worked on it keep their record.'),
    holding('index', 'Search index', 'vector', 24_610, 25_165_824, null, 'Derived from documents and memories on this node: deleting one of them drops its vectors. Never replicated.'),
    holding('workspaces', 'Workspaces', 'workspace', 37, 5_402_198_016, RUNTIME, 'On this node only; nothing replicates a workspace. Removing one gives up resuming, exporting and restoring its session.'),
    holding('harness', 'Harness state', 'session', 211, 1_133_510_656, RUNTIME, 'On this node only. Removing a session’s directory gives up resuming it; the rows the node mirrored from the harness stay with the session.'),
  ],
}

const build = (over) => ({
  id: 'b-1',
  repo_path: '/var/lib/tracon/repos/github.com/op/orbit',
  kind: 'base',
  source_ref: 'main',
  source_commit: '3c9d0e1f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d',
  image: 'localhost/tracon-repo-orbit@sha256:5e1c…',
  status: 'ready',
  log_tail: '',
  error: '',
  warnings: [],
  started_ms: -7200000,
  finished_ms: -7080000,
  ...over,
})
const REPO_ENV = {
  can_build: true,
  presets: [
    ['crates', ['crates.io', 'index.crates.io', 'static.crates.io']],
    ['npm', ['registry.npmjs.org']],
    ['pypi', ['pypi.org', 'files.pythonhosted.org']],
    ['packagist', ['packagist.org', 'repo.packagist.org', 'api.github.com', 'codeload.github.com']],
    ['github', ['github.com', 'api.github.com', 'codeload.github.com']],
  ],
  entries: [
    {
      entry: { path: 'github.com/op/orbit', dockerfile: '.devcontainer/Dockerfile', checks: ['cargo nextest run --workspace', 'bun run --cwd spa check'], timeout_secs: 1800, prepare: ['cargo fetch --locked', 'bun install --frozen-lockfile --cwd spa'], egress: ['crates', 'npm'], session_egress: true },
      builds: [build({}), build({ id: 'b-2', kind: 'session:claude', image: 'localhost/tracon-repo-orbit-claude@sha256:9a0f…' })],
    },
    {
      entry: { path: '/home/op/src/consulta', image: 'ghcr.io/op/python-toolchain@sha256:4f1a9c2e7b6d5e8a3c0f9b2d1e4a7c6b5d8e0f3a2c1b4d7e6f9a8c5b2e1d0f3a', prepare: ['uv sync --frozen'], egress: ['pypi'] },
      builds: [],
    },
  ],
}

const MANIFEST = {
  channel: 'personal',
  items: [
    { channel: 'personal', kind: 'skill', name: 'release-notes', source: '/home/op/skills/release-notes', digest: '9f2c1a7e44b0', body: '', warnings: [], imported_ms: -86400000 * 3 },
    { channel: 'personal', kind: 'agent', name: 'reviewer', source: '/home/op/skills/agents/reviewer.md', digest: 'c40e88a1d2f3', body: '', warnings: ['names a tool the harness does not offer: WebFetch'], imported_ms: -86400000 },
    { channel: 'personal', kind: 'instruction', name: 'house-style', source: '', digest: '', body: 'Prefer small commits with conventional-commit subjects. Ask before adding a dependency. Run `just check` before submitting a review.', warnings: [], imported_ms: -3600000 },
  ],
  recorded: { channel: 'personal', revision: 6, digest: '4e1d0c9b8a7f6e5d', skills: [], instructions: [], agents: [], plugins: [], lsp: [], formatters: [], providers: [], policy_revision: 'v12' },
  next: {
    channel: 'personal',
    revision: 7,
    digest: '7b3a9f0e2c1d4e5f',
    skills: [{ name: 'release-notes', description: 'Draft release notes from merged PRs', source: '/home/op/skills/release-notes', digest: '9f2c1a7e44b0' }],
    instructions: [{ name: 'house-style', body: '…' }],
    agents: [{ name: 'reviewer', body: '…' }],
    plugins: [],
    lsp: [{ name: 'rust-analyzer', command: ['rust-analyzer'] }, { name: 'typescript', command: ['typescript-language-server', '--stdio'] }],
    formatters: [{ name: 'rustfmt', command: ['rustfmt'] }, { name: 'prettier', command: ['prettier'] }],
    providers: ['anthropic', 'openrouter'],
    policy_revision: 'v12',
  },
  error: null,
  baked_plugins: ['@tracon/opencode-orientation'],
}

const TRANSFERS = {
  transfers: [
    { id: 't-1', candidate_id: 'cand-7f3e9a1c', channel: 'work', origin_node: PEER, target_node: SELF, created_ms: -5400000, files: 14, documents: 2, memories: 0, note: 'Continue the flaky-test fix on a machine with more cores.', import_state: null, session_id: null },
    { id: 't-2', candidate_id: 'cand-2b8d40e7', channel: 'personal', origin_node: GONE, target_node: null, created_ms: -86400000 * 3, files: 3, documents: 0, memories: 1, note: '', import_state: 'failed', session_id: null },
  ],
}

const PUSH_DEVICES = {
  devices: [
    { id: 'd-1', user_agent: 'Chrome 141 on Linux', created_ms: -86400000 * 20, last_ok_ms: -900000, fail_count: 0, local: true, mine: true },
    { id: 'd-2', user_agent: 'Safari on iPhone (installed web app)', created_ms: -86400000 * 40, last_ok_ms: -3600000 * 5, fail_count: 0, local: false, mine: false },
    { id: 'd-3', user_agent: 'Mozilla/5.0 (Linux; Android 15; Pixel 9 Pro Build/AP4A.250105.002; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/141.0.7390.122 Mobile Safari/537.36', created_ms: -86400000 * 90, last_ok_ms: null, fail_count: 7, local: false, mine: false },
  ],
}

const EXTERNAL = { enabled: true, channels: ['personal', 'work', 'client-hdr'], lanes: [], stopped: [] }

// Everything Settings reads, in its populated form.
const BASE = {
  '/api/nodes': NODES,
  '/api/node': selfNode(),
  '/api/mesh': MESH,
  '/api/channels': CHANNELS,
  '/api/providers': PROVIDERS,
  '/api/credentials': CREDENTIALS,
  '/api/config': CONFIG,
  '/api/authority/grants': AUTHORITY,
  '/api/admin/access': ACCESS,
  '/api/admin/mesh': ADMIN_MESH,
  '/api/mesh/rollups': ROLLUPS,
  '/api/admin/policy': POLICY,
  '/api/admin/maintenance': MAINTENANCE,
  '/api/maintenance/data': DATA,
  '/api/repos/environments': REPO_ENV,
  '/api/manifest': MANIFEST,
  '/api/transfers': TRANSFERS,
  '/api/push/subscriptions': PUSH_DEVICES,
  '/api/external': EXTERNAL,
}
const api = (over = {}) => ({ ...BASE, ...over })
const fail = (status, message) => ({ status, body: { error: { code: status, message } } })
const never = () => new Promise(() => {})

// A browser push stack the page can enrol against, without a real push
// service: permission, the worker's readiness, subscribe and the existing
// subscription are each set per state. A string, so it serialises as is.
const pushStub = ({ permission = 'granted', subscribed = false, subscribeError = null, noPush = false } = {}) => `(() => {
  const sub = {
    endpoint: 'https://fcm.googleapis.com/fcm/send/eXaMpLe-endpoint',
    toJSON() { return { endpoint: this.endpoint, keys: { p256dh: 'BExampleP256dhKey', auth: 'ExampleAuth' } } },
    unsubscribe: async () => { has = false; return true },
  }
  let has = ${subscribed}
  const reg = {
    pushManager: {
      getSubscription: async () => (has ? sub : null),
      subscribe: async () => {
        ${subscribeError ? `const e = new Error(${JSON.stringify(subscribeError[1])}); e.name = ${JSON.stringify(subscribeError[0])}; throw e` : 'has = true; return sub'}
      },
    },
  }
  const sw = {
    ready: Promise.resolve(reg),
    controller: null,
    register: async () => reg,
    getRegistration: async () => reg,
    addEventListener() {},
    removeEventListener() {},
  }
  Object.defineProperty(navigator, 'serviceWorker', { value: sw, configurable: true })
  ${noPush ? "delete window.PushManager; Object.defineProperty(window, 'PushManager', { value: undefined, configurable: true }); delete window.PushManager" : ''}
  if (window.Notification) {
    Object.defineProperty(Notification, 'permission', { get: () => ${JSON.stringify(permission === 'granted' ? 'default' : permission)}, configurable: true })
    Notification.requestPermission = async () => ${JSON.stringify(permission)}
  }
})()`

// The desktop shell's bridge, answering the commands Settings sends it.
const tauriStub = ({ platform = 'macos', update, setup, prefs }) => `(() => {
  const answers = ${JSON.stringify({
    desktop_update_status: update,
    desktop_setup_status: setup,
    desktop_preferences: prefs,
    desktop_managed_local: true,
  })}
  window.isTauri = true
  let cb = 0
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    convertFileSrc: (p) => p,
    invoke: async (cmd) => (cmd in answers ? answers[cmd] : null),
  }
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} }
  void ${JSON.stringify(platform)}
})()`
const SETUP = {
  platform: 'macos',
  owner: 'service',
  node_version: '0.29.0',
  sidecar_version: '0.29.0',
  cli_version: '0.28.0',
  cli_path: '/usr/local/bin/tracon',
  path_hint: null,
  service_installed: true,
  service_running: true,
  service_failing: false,
  service_error: null,
  podman: '5.6.1',
  machine: 'running',
  machine_error: null,
}

const click = (page, name, opts = {}) => page.getByRole('button', { name, exact: opts.exact ?? false }).first().click()
const scrollTo = (page, text) => page.getByText(text, { exact: false }).first().scrollIntoViewIfNeeded()
// A full-page shot taken while scrolled pins the sticky section nav and the
// rail mid-page; scroll back once the interaction is done.
const S = (id, route, title, rest = {}) => {
  const state = { id: `settings-${id}`, area: 'settings', route, title, api: api(rest.over), ...rest }
  delete state.over
  if (rest.act && rest.full !== false) {
    state.act = async (page, ctx) => {
      await rest.act(page, ctx)
      await page.evaluate(() => window.scrollTo(0, 0))
    }
  }
  return state
}

export default [
  // --- General ---------------------------------------------------------------
  S('general', '/settings', 'General: appearance and versions', {
    note: 'Theme segmented control and the per-node version list, including a peer last reported on an older release.',
  }),
  S('general-desktop', '/settings', 'General in the desktop app', {
    note: 'Desktop-only cards: app preferences (macOS toggles), version and updates with an update ready and an older CLI to replace.',
    init: tauriStub({ update: { state: 'available', current_version: '0.28.0', available_version: '0.29.0' }, setup: SETUP, prefs: { open_window_at_launch: false, cmd_q_quits: true, hide_dock_when_closed: true } }),
  }),
  S('general-desktop-failing', '/settings', 'Desktop app: service keeps exiting, update failed', {
    note: 'Linux hides the ⌘Q and dock toggles; the service-failing reason and a long update failure message.',
    init: tauriStub({
      platform: 'linux',
      update: { state: 'failed', current_version: '0.29.0', message: 'signature verification failed for tracon_0.29.1_amd64.AppImage.tar.gz: the update manifest names a key this build does not trust (minisign key id 4F1A9C2E7B6D5E8A)' },
      setup: { ...SETUP, platform: 'linux', owner: 'none', node_version: null, service_running: false, service_failing: true, service_error: 'Error: bind 127.0.0.1:7420: address already in use (os error 98) — another process holds the node port' },
      prefs: { open_window_at_launch: true, cmd_q_quits: false, hide_dock_when_closed: false },
    }),
  }),

  // --- Connections -----------------------------------------------------------
  S('connections', '/settings#connections', 'Connections: providers, forge tokens, credential copies', {
    note: 'One provider of each state: subscription connected (declared models open), device-code sign-in pending, API key connected (5 models, collapsed), API key failed. Node selector with an unavailable peer.',
  }),
  S('connections-empty', '/settings#connections', 'Connections: nothing connected yet', {
    note: 'Onboarding call to action, no forge tokens, no credential copies card.',
    over: {
      '/api/nodes': [selfNode({ providers: [], models: [] })],
      '/api/providers': [
        { ...P_ANTHROPIC, state: 'disconnected', kind: null, identity: null, expires_ms: null, channels: [], updated_ms: null },
        { ...P_CODEX, state: 'disconnected', url: null, completion: null, completion_note: null, device_code: null, channels: [] },
      ],
      '/api/credentials': { credentials: [] },
      '/api/config': { ...CONFIG, providers: {} },
    },
  }),
  S('connections-add-chooser', '/settings#connections', 'Add a provider: chooser', {
    act: (page) => click(page, 'Add a provider'),
  }),
  S('connections-add-custom', '/settings#connections', 'Add a provider: API key / custom form', {
    note: 'Custom provider form with a create error from the node.',
    over: { 'POST /api/credentials/import': fail(422, 'a credential named openrouter already exists; remove it first or choose another name') },
    act: async (page) => {
      await click(page, 'Add a provider')
      await click(page, 'API key / custom')
      await page.getByPlaceholder('openrouter', { exact: true }).fill('openrouter')
      await page.getByPlaceholder('https://openrouter.ai/api/v1').fill('https://openrouter.ai/api/v1')
      await page.locator('input[type=password]').first().fill('sk-or-v1-0000')
      await click(page, 'Save provider')
    },
  }),
  S('connections-editing', '/settings#connections', 'Connections: editing channels, replacing a token, confirming a share', {
    note: 'Provider channel editor, GitHub token editor (with the peer-copy warning), GitLab remove confirm is not reachable without a token, and a sealed-share confirmation.',
    act: async (page) => {
      await click(page, 'edit channels')
      await click(page, 'Replace')
      const sel = page.locator('.share select').first()
      await sel.selectOption({ index: 1 })
      await click(page, 'Review share')
    },
  }),
  S('connections-peer', `/settings?node=${PEER}#connections`, 'Connections: managing a peer', {
    note: 'Peer provider card scope note, Disconnect on <peer>.',
  }),
  S('connections-peer-unavailable', `/settings?node=${GONE}#connections`, 'Connections: an unavailable peer selected', {}),
  S('connections-long', '/settings#connections', 'Connections: long identities, errors and names', {
    note: 'Long account identity, long provider name, long failure message, many channels.',
    over: {
      '/api/providers': [
        { ...P_ANTHROPIC, identity: 'platform-engineering-shared-automation-account@subsidiary.example-enterprise.com', channels: ['personal', 'work', 'client-hdr', 'oss-maintenance', 'infrastructure-on-call'] },
        { ...P_OPENROUTER, name: 'openrouter-eu-west-dedicated-capacity-pool' },
        { ...P_OPENAI_FAILED, error: 'upstream refused the key: 401 {"error":{"message":"Incorrect API key provided: sk-proj-************************************************************Xq9a. You can find your API key at https://platform.openai.com/account/api-keys.","type":"invalid_request_error","code":"invalid_api_key"}}' },
      ],
      '/api/credentials': {
        credentials: [
          ...CREDENTIALS.credentials,
          { name: 'aws-deploy-role-production-eu-central-1', kind: 'env', provider: null, channels: ['work'], nodes: [PEER, GONE], identity: 'arn:aws:iam::123456789012:role/tracon-deploy-production-eu-central-1', expires_ms: null, env_keys: ['AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY', 'AWS_SESSION_TOKEN', 'AWS_REGION', 'AWS_ROLE_ARN'] },
        ],
      },
    },
  }),

  // --- Channels --------------------------------------------------------------
  S('channels', '/settings#channels', 'Channels: models, exhaustion policy, archived, meters, customization', {
    since: '#384',
    note: 'Each open channel row has the new "When a provider is exhausted" select: fall back then wait (fallback chosen), pause, and a fallback policy with no model ("not saved until…"). A model no node offers (haiku-legacy). Meters at near/at with unmetered turns.',
  }),
  S('channels-exhaustion-draft', '/settings#channels', 'Channels: choosing a fallback before its model', {
    since: '#384',
    note: 'Switching personal-less "work" to a fallback policy holds it as a draft until a fallback model is picked.',
    act: async (page) => {
      await page.locator('.exh select').nth(1).selectOption('fallback')
    },
  }),
  S('channels-model-picker-open', '/settings#channels', 'Channels: fallback model picker open', {
    note: 'The fallback ModelPicker list for client-hdr (no fallback chosen yet).',
    since: '#384',
    act: async (page) => {
      await page.locator('.ch').nth(3).locator('.exh input[role=combobox]').click()
    },
    sizes: ['desktop'],
  }),
  S('channels-delete-confirm', '/settings#channels', 'Channels: confirm deleting an archived channel', {
    act: async (page) => {
      await page.locator('.ch.off').getByRole('button', { name: 'Delete' }).click()
      await scrollTo(page, 'Archived channels')
    },
  }),
  S('channels-empty', '/settings#channels', 'Channels: none yet', {
    note: 'Customization still renders, with an empty channel select, against a hard-coded "personal" channel.',
    over: { '/api/channels': [], '/api/manifest': { ...MANIFEST, items: [], recorded: null, next: { ...MANIFEST.next, revision: 1, skills: [], instructions: [], agents: [] } } },
  }),
  S('channels-no-models', '/settings#channels', 'Channels: no node offers a model', {
    over: {
      '/api/nodes': [selfNode({ models: [], providers: [] }), peerNode({ models: [] })],
      '/api/channels': CHANNELS.slice(0, 2),
      '/api/manifest': { ...MANIFEST, items: [], next: { ...MANIFEST.next, lsp: [], formatters: [] }, recorded: null, baked_plugins: [] },
    },
  }),
  S('channels-manifest-refused', '/settings#channels', 'Channels: manifest that will not build', {
    note: 'Customization card: "will not build", the refusal reason, and item warnings.',
    over: {
      '/api/manifest': {
        ...MANIFEST,
        next: null,
        error: 'agent "reviewer" names model openrouter/claude-opus-legacy-preview-2025-03, which no provider on this node declares; remove the agent or declare the model',
        items: [
          ...MANIFEST.items,
          { channel: 'personal', kind: 'skill', name: 'database-migration-review-with-rollback-plan', source: '/home/op/src/internal-skills/packages/database-migration-review-with-rollback-plan#4f1a9c2e', digest: 'aa01bc23de45', body: '', warnings: ['script scripts/check.sh is not executable', 'SKILL.md is 61 KB; a session reads all of it on every launch'], imported_ms: -600000 },
        ],
      },
      '/api/manifest/text': fail(500, 'unreachable'),
    },
    act: (page) => scrollTo(page, 'Customization'),
  }),
  S('channels-long', '/settings#channels', 'Channels: many channels with long names', {
    over: {
      '/api/channels': [
        ...CHANNELS,
        ...['infrastructure-on-call-rotation-eu', 'oss-maintenance', 'client-acme-platform-modernisation-2026', 'docs', 'experiments', 'reading-list'].map((name, i) => ({
          name,
          nodes: [SELF],
          bindings: i % 2 ? { exhaustion: { policy: 'pause' } } : { phases: { plan: { model: 'anthropic/sonnet' } } },
          ceiling: ceiling(120000 * (i + 1), i % 3 ? 2000000 : null, i % 3 ? 'under' : 'none'),
          archived: null,
        })),
      ],
    },
  }),

  // --- Repositories ----------------------------------------------------------
  S('repositories', '/settings#repositories', 'Repositories: two entries, built images', {
    note: 'Dockerfile entry with base and session images ready and sessions given egress; a pinned image entry with a long digest.',
  }),
  S('repositories-builds', '/settings#repositories', 'Repositories: failed, building and warning builds', {
    note: 'Three entries (collapsed by default) — expanded here: failed build with output open, a building one, missing-tool warnings, the github upload caution.',
    over: {
      '/api/repos/environments': {
        ...REPO_ENV,
        entries: [
          {
            entry: { ...REPO_ENV.entries[0].entry, egress: ['crates', 'npm', 'github'] },
            builds: [
              build({ status: 'failed', error: 'step 7/12 RUN cargo install cargo-nextest --locked: exit status 101', finished_ms: -60000, log_tail: '#7 41.20    Compiling cargo-nextest v0.9.104\n#7 58.03 error: linker `cc` not found\n#7 58.03   |\n#7 58.03   = note: No such file or directory (os error 2)\n#7 58.10 error: could not compile `cargo-nextest` (bin "cargo-nextest") due to 1 previous error\n#7 ERROR: process "/bin/sh -c cargo install cargo-nextest --locked" did not complete successfully: exit code: 101' }),
            ],
          },
          {
            entry: { path: 'github.com/op/site', dockerfile: 'Dockerfile.dev', context: '.', prepare: ['composer install --no-interaction', 'npm ci'], egress: ['packagist', 'npm'], checks: [] },
            builds: [build({ id: 'b-3', repo_path: '/var/lib/tracon/repos/github.com/op/site', status: 'building', source_commit: 'b1e2d3c4a5f60718293a4b5c6d7e8f9012345678', finished_ms: null })],
          },
          {
            entry: { path: '/home/op/src/consulta', dockerfile: '.devcontainer/Dockerfile', prepare: ['uv sync --frozen', 'just setup-db'], egress: ['pypi'] },
            builds: [build({ id: 'b-4', repo_path: '/home/op/src/consulta', warnings: ['just', 'psql'] })],
          },
        ],
      },
    },
    act: async (page) => {
      for (const d of await page.locator('details.entry').all()) await d.evaluate((el) => (el.open = true))
      await page.locator('.builds details').first().evaluate((el) => (el.open = true))
    },
  }),
  S('repositories-empty', '/settings#repositories', 'Repositories: no entries', {
    over: { '/api/repos/environments': { ...REPO_ENV, entries: [] } },
  }),
  S('repositories-new-invalid', '/settings#repositories', 'Repositories: a new entry with problems', {
    note: 'Added a third entry: the two saved ones collapse, the new one is open with no repository named and a pinned image left empty; the first problem shows beside the disabled Save.',
    act: async (page) => {
      await click(page, '+ Add a repository')
      await page.locator('details.entry').last().locator('select').first().selectOption('image')
    },
  }),
  S('repositories-new-collapses', '/settings#repositories', 'Repositories: typing a path into a third entry collapses it', {
    note: 'Bug: with three or more entries, `open={!form.path || forms.length <= 2}` closes the new entry on its first keystroke.',
    act: async (page) => {
      await click(page, '+ Add a repository')
      await page.locator('details.entry').last().locator('input').first().fill('github.com/op/orbit')
    },
  }),
  S('repositories-error', '/settings#repositories', 'Repositories: table could not be read', {
    over: { '/api/repos/environments': fail(500, 'node.toml does not parse: TOML parse error at line 48, column 1: duplicate key `path` in table `repo`') },
  }),
  S('repositories-loading', '/settings#repositories', 'Repositories: reading', {
    over: { '/api/repos/environments': never },
    sizes: ['desktop'],
  }),

  // --- Devices & notifications ----------------------------------------------
  S('devices', '/settings#devices', 'Devices: push off, three registered devices', {
    note: 'One device is this browser, one fails, one has a long user agent.',
    init: pushStub(),
  }),
  S('devices-on', '/settings#devices', 'Devices: push on, test accepted', {
    init: pushStub({ subscribed: true }),
    over: { 'POST /api/push/test': { sent: [{ id: 'd-1', outcome: 'accepted', service_accepted: true }, { id: 'd-2', outcome: 'accepted', service_accepted: true }, { id: 'd-3', outcome: 'gone (410)', service_accepted: false }] } },
    act: (page) => click(page, 'Send a test'),
  }),
  S('devices-denied', '/settings#devices', 'Devices: enrolment stopped at the permission prompt', {
    since: '#403',
    note: 'The failure note is now red and names the stage: notifications blocked for this site.',
    init: pushStub({ permission: 'denied' }),
    act: (page) => page.getByRole('checkbox').first().click(),
  }),
  S('devices-push-service', '/settings#devices', 'Devices: the browser push service refused', {
    since: '#403',
    init: pushStub({ subscribeError: ['AbortError', 'Registration failed - push service error'] }),
    act: (page) => page.getByRole('checkbox').first().click(),
  }),
  S('devices-node-failed', '/settings#devices', 'Devices: the node did not record the device', {
    since: '#403',
    note: 'A 500 from POST /api/push/subscriptions: "its storage failed: …".',
    init: pushStub(),
    over: { 'POST /api/push/subscriptions': fail(500, 'database is locked') },
    act: (page) => page.getByRole('checkbox').first().click(),
  }),
  S('devices-unsupported', '/settings#devices', 'Devices: push not available, none registered', {
    init: pushStub({ noPush: true }),
    over: { '/api/push/subscriptions': { devices: [] } },
  }),

  // --- Mesh ------------------------------------------------------------------
  S('mesh', '/settings#mesh', 'Mesh: paired, members, hub summaries', {
    note: 'Hub row, three members (one on an older release with mismatched app/wire/policy), invite and share cards, hub summaries current/partial.',
  }),
  S('mesh-unreachable', '/settings#mesh', 'Mesh: hub unreachable with errors and backlog', {
    over: {
      '/api/mesh': { ...MESH, hub: { state: 'unreachable', since_ms: -1500000 }, last_ok_ms: -1500000, queued: 37, delivered_since_reconnect: 0, undecryptable: 2, held: 1, last_error: 'connect wss://hub.example.net/v1/relay: tls handshake eof after 10 s', last_refusal: 'hub refused frame: node 9f31c6a8 is not in the directory for channel work' },
    },
    full: false,
  }),
  S('mesh-unpaired-locked', '/settings#mesh', 'Mesh: not paired, admin locked', {
    note: 'Join form, first-node setup expanded, administrator access asking for a token, mesh administration with nothing to show.',
    over: {
      '/api/mesh': MESH_OFF,
      '/api/nodes': [selfNode()],
      '/api/admin/access': { authenticated: false, token_configured: true, local: true },
      '/api/admin/mesh': fail(401, 'administrator access required'),
    },
    act: (page) => page.locator('details.first-node').evaluate((el) => (el.open = true)),
  }),
  S('mesh-first-node-prepared', '/settings#mesh', 'Mesh: first node prepared, join log failed', {
    note: 'Prepared first node with the hub admission value; a failed join attempt log above.',
    over: {
      '/api/mesh': MESH_OFF,
      '/api/nodes': [selfNode()],
      '/api/admin/access': { authenticated: false, token_configured: false, local: true },
      '/api/admin/mesh': fail(401, 'administrator access required'),
      'POST /api/mesh/init': { hub_url: 'https://hub.example.net', admit_with: 'tracon-admit:v1:q3x9Lw0b8Yk2mH7tR1cV5nP4sJ6dA0fEz2uGy8oIkM4hN1bQe7rT5cW3vX0aS9dF', restart_required: true },
      'POST /api/mesh/enroll': {},
      '/api/mesh/enroll': { lines: ['resolving hub.example.net', 'fetching invitation 7Q4-K2M', 'checking hub identity'], done: true, error: 'invitation 7Q4-K2M has expired; ask for a new one with `tracon mesh invite`', channels: [], restart_required: false },
    },
    act: async (page) => {
      await page.getByPlaceholder('full invitation URL').fill('https://hub.example.net/join#7Q4-K2M')
      await click(page, 'Join hub')
      await page.waitForTimeout(300)
      await page.locator('details.first-node').evaluate((el) => (el.open = true))
      await page.getByPlaceholder('https://hub.example.com').fill('https://hub.example.net')
      await click(page, 'Prepare first node')
    },
  }),
  S('mesh-invitation-received', '/settings#mesh', 'Mesh: invitation answered, fingerprint check', {
    over: {
      'POST /api/admin/mesh/invitations': {
        code: 'inv-7q4k2m',
        display_code: '7Q4-K2M',
        url: 'https://hub.example.net/join#7Q4-K2M.eyJodWIiOiJodWIuZXhhbXBsZS5uZXQiLCJleHAiOjE3OTk5OTk5OTl9',
        qr_svg: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 29 29" width="160" height="160"><rect width="29" height="29" fill="#fff"/><path d="M1 1h7v7H1zM21 1h7v7h-7zM1 21h7v7H1zM10 3h2v2h-2zM13 5h3v1h-3zM10 10h9v9h-9zM21 12h2v5h-2zM3 11h4v2H3zM12 22h5v3h-5zM20 20h3v3h-3zM24 24h3v3h-3z" fill="#000"/></svg>',
        channels: ['@mesh', 'work'],
        expires_at: Date.now() + 900000,
        state: 'received',
        received: { node_id: 'e5a1…', name: 'ci-runner-03' },
        received_fingerprint: 'SHA256:mT4p Q9xa 2LvK 8dRz 0bWc',
        own_fingerprint: 'SHA256:q3x9 Lw0b 8Yk2 mH7t R1cV',
      },
    },
    act: async (page) => {
      await page.locator('.choices').first().getByLabel(/work/).check()
      await click(page, 'Create invitation for selected channels')
      await scrollTo(page, 'Fingerprint check required')
    },
  }),
  S('mesh-confirms', '/settings#mesh', 'Mesh: unpair and remove-member confirmations', {
    act: async (page) => {
      await click(page, 'Unpair', { exact: true })
      await click(page, 'Remove member…')
    },
  }),

  // --- Permissions & policies ------------------------------------------------
  S('policies', '/settings#policies', 'Policies: signed policy, rollouts, local grants', {
    note: 'Installed and running policy, rollout history with applied/offline/rejected targets, grants: active with revision and expiry, terminal, long target deny, revoked.',
  }),
  S('policies-edit', '/settings#policies', 'Policies: editing with a signed preview', {
    over: { 'POST /api/admin/policy/preview': (req) => ({ ...bundle(12, 'f00dfacec0ffee1234567890abcdef1234567890abcdef1234567890abcdef12'), toml: req.body.toml }) },
    act: async (page) => {
      await page.locator('details.edit-policy').evaluate((el) => (el.open = true))
      await click(page, 'Preview & sign')
      await page.locator('.target input').first().check()
    },
  }),
  S('policies-uninitialized', '/settings#policies', 'Policies: nothing installed, untrusted, no grants', {
    over: {
      '/api/admin/policy': { ...POLICY, installed: null, running: { version: 0, rule_count: 0, trusted: false }, trust_identity: null, installation: null, rollouts: [], signing_key_present: false },
      '/api/authority/grants': { policy: { version: 0, rules: [], trusted: false }, grants: [] },
    },
    act: (page) => page.locator('details.initialize').evaluate((el) => (el.open = true)),
  }),
  S('policies-installed-error', '/settings#policies', 'Policies: installed files fail verification, apply conflict', {
    over: {
      '/api/admin/policy': { ...POLICY, installed_error: 'signature does not verify against trust identity ed25519:7HqP4mZ0bN9xVv2Kc8sTq1yWf6eR3uLa5jDg0oIhc2Lw (policy.toml modified at 2026-10-07 22:14)' },
    },
  }),
  S('policies-error', '/settings#policies', 'Policies: status unavailable, admin locked', {
    over: {
      '/api/admin/access': { authenticated: false, token_configured: true, local: false },
      '/api/admin/policy': fail(401, 'administrator access required'),
    },
  }),
  S('policies-grant-invalid', '/settings#policies', 'Policies: new grant with nothing filled in', {
    note: 'Review grant with no target or reason raises the page-level error banner, far above the form.',
    act: async (page) => {
      await click(page, 'Review grant')
    },
  }),
  S('policies-grant-confirm', '/settings#policies', 'Policies: terminal grant, then its confirmation', {
    note: 'Terminal grant explanation, advanced scope open, then the exact-grant confirmation.',
    act: async (page) => {
      const form = page.locator('.grid').last()
      await page.locator('select').filter({ hasText: 'ticket transition' }).first().selectOption('terminal')
      await page.locator('details.advanced-grant').evaluate((el) => (el.open = true))
      await page.getByPlaceholder('limit to one session id').fill('s-run')
      await page.getByPlaceholder('github:owner/repo:pr:42').fill('terminal:s-run:/workspaces/s-run/orbit')
      await page.getByPlaceholder('why this exact scoped action is needed').fill('Reproduce the flaky integration test by hand.')
      void form
      await scrollTo(page, 'A terminal grant opens')
    },
  }),
  S('policies-grant-confirmed-step', '/settings#policies', 'Policies: confirm this exact local grant', {
    act: async (page) => {
      await page.getByPlaceholder('github:owner/repo:pr:42').fill('github:op/orbit:pr:418')
      await page.getByPlaceholder('why this exact scoped action is needed').fill('Merge after the release PR lands.')
      await click(page, 'Review grant')
      await scrollTo(page, 'Confirm this exact local grant')
    },
  }),

  // --- Data (#402) -------------------------------------------------------------
  S('data', '/settings#data', 'Data: what the node holds, kind by kind', {
    since: '#402',
    note: 'Totals line, per-kind counts and sizes (GB workspaces, a single memory), delete links and propagation text.',
  }),
  S('data-empty', '/settings#data', 'Data: a fresh node', {
    since: '#402',
    over: { '/api/maintenance/data': { database_bytes: 98304, total_bytes: 98304, holdings: DATA.holdings.map((h) => ({ ...h, count: 0, bytes: 0 })) } },
  }),
  S('data-error', '/settings#data', 'Data: inventory failed', {
    since: '#402',
    over: { '/api/maintenance/data': fail(500, 'walking /home/op/.local/state/tracon/workspaces: permission denied (os error 13) at workspaces/s-8c1f/node_modules/.cache') },
  }),
  S('data-loading', '/settings#data', 'Data: counting', {
    since: '#402',
    over: { '/api/maintenance/data': never },
    sizes: ['desktop'],
  }),

  // --- Maintenance -----------------------------------------------------------
  S('maintenance', '/settings#maintenance', 'Maintenance: configuration, access, boundary, service, harness, transfers', {
    note: 'Two harnesses (opencode image stale), capabilities (restart unavailable), external harness MCP lines per channel, transfer inbox.',
    act: (page) => page.locator('details').filter({ hasText: 'Import a session' }).first().evaluate((el) => (el.open = true)).catch(() => {}),
  }),
  S('maintenance-refused', '/settings#maintenance', 'Maintenance: boundary refused, harness mismatch', {
    note: 'Node refused because the Podman gateway service is stopped (#377 starts it when found stopped; this is the case it could not), a harness version mismatch, service state unknown, setup check list.',
    over: {
      '/api/nodes': [
        selfNode({
          state: 'refused',
          failed_check: 'gateway',
          failed_detail: 'tracon-gateway.service is inactive (failed) and did not start: podman exited 125: network tracon-boundary not found',
          harnesses: [{ ...HARNESSES[0], found: '2.4.9', mismatch: true }, { ...HARNESSES[1], image_state: 'missing', found: null }],
        }),
        peerNode(),
      ],
      '/api/admin/maintenance': {
        ...MAINTENANCE,
        active_sessions: 0,
        service: { ...MAINTENANCE.service, state: { state: 'unknown', detail: 'systemctl --user is-active returned no answer within 5 s' }, restart: cap(true, 'No sessions are active.', 'The node answers again within a few seconds of a restart.') },
        boundary: selfNode({ state: 'refused' }),
      },
      'POST /api/boundary/setup': { state: 'refused', checks: { checks: [
        { id: 'podman', ok: true, detail: 'podman 5.6.1 rootless' },
        { id: 'network', ok: false, detail: 'network tracon-boundary not found; recreated' },
        { id: 'gateway', ok: false, detail: 'tracon-gateway.service failed to start: port 3128 in use by squid (pid 2231)' },
        { id: 'image:claude', ok: true, detail: 'localhost/tracon-claude:2.5.0 present' },
      ] } },
    },
    act: async (page) => {
      await click(page, 'Prepare isolated runtime')
      await click(page, 'Restart this serving node…')
    },
  }),
  S('maintenance-config-dirty', '/settings#maintenance', 'Maintenance: configuration edited, not yet saved', {
    note: 'Long node name typed, review cap changed: Save configuration enabled.',
    act: async (page) => {
      await page.locator('label').filter({ hasText: 'Node name' }).locator('input').fill('laptop-orbit-and-consulta-development-workstation')
      await page.locator('label').filter({ hasText: 'Review cap (lines)' }).locator('input').fill('6000')
    },
  }),
  S('maintenance-config-error', '/settings#maintenance', 'Maintenance: node.toml unreadable, external off', {
    over: { '/api/config': fail(500, 'TOML parse error at line 48, column 1\n   |\n48 | path = "github.com/op/orbit"\n   | ^\nduplicate key `path` in table `repo`') },
  }),
  S('maintenance-token-confirm', '/settings#maintenance', 'Maintenance: create or rotate operator access', {
    act: async (page) => {
      await page.getByPlaceholder('https://node.tailnet.ts.net').fill('https://laptop.tail0a1b2.ts.net')
      await click(page, 'Create or rotate token')
      await scrollTo(page, 'Create or replace operator access?')
    },
    full: false,
  }),
  S('maintenance-storage', '/settings#maintenance', 'Maintenance: reclaimable storage found', {
    over: {
      'POST /api/maintenance/storage': {
        applied: false,
        items: [
          { kind: 'volume', name: 'tracon-ws-s-8c1f0e2a', remove: true, reason: 'session s-8c1f0e2a archived 9 days ago' },
          { kind: 'directory', name: '/home/op/.local/state/tracon/workspaces/s-3be91d07-orbit-fix-flaky-integration-test-on-ci', remove: true, reason: 'session archived 2 days ago' },
          { kind: 'volume', name: 'tracon-cache-cargo-orbit', remove: true, reason: 'dependency cache; rebuilt when next needed' },
          { kind: 'volume', name: 'tracon-ws-s-run', remove: false, reason: 'session s-run is running' },
        ],
      },
    },
    act: async (page) => {
      await page.getByLabel(/Include dependency caches/).check()
      await click(page, 'Find reclaimable storage')
      await page.waitForTimeout(200)
      await page.getByRole('button', { name: 'Remove 3…', exact: true }).click()
      await page.locator('details').filter({ hasText: 'Kept ·' }).evaluate((el) => (el.open = true))
      await scrollTo(page, 'Runtime storage')
    },
  }),
  S('maintenance-external-stopped', '/settings#maintenance', 'Maintenance: external broker access stopped on one channel', {
    over: {
      '/api/channels': CHANNELS.map((c) => (c.name === 'work' ? { ...c, bindings: { ...c.bindings, external_stopped: true } } : c)),
    },
    act: async (page) => {
      await page.locator('.ext').first().getByRole('button', { name: 'Stop broker access' }).click()
      await scrollTo(page, 'Your own harness')
    },
    full: false,
  }),

  // --- Cross-cutting ---------------------------------------------------------
  S('remote', '/settings#maintenance', 'Reached remotely: banners and disabled controls', {
    note: 'Not loopback: the remote banner, disabled configuration, "blocked" notes, remote-admin note.',
    over: {
      '/api/nodes': [selfNode({ loopback: false }), peerNode(), goneNode()],
      '/api/admin/access': { authenticated: true, token_configured: true, local: false },
      '/api/admin/maintenance': { ...MAINTENANCE, local: false },
    },
  }),
  S('awake-held', '/settings', 'Rail: keeping the machine awake, slept recently', {
    since: '#379',
    note: 'Rail footer lines: "keeping awake · <reason>" and "slept N min · ago". asleep_ms is a duration sent as a string here: the runner stamps every small *_ms number as an offset from now.',
    over: { '/api/awake': { held: true, reason: '2 sessions working on personal and work', method: 'logind', error: null, last_suspend: { woke_ms: -900000, asleep_ms: '2520000' } } },
    sizes: ['desktop'],
    full: false,
  }),
  S('awake-error', '/settings', 'Rail: cannot keep the machine awake', {
    since: '#379',
    over: { '/api/awake': { held: false, reason: null, method: null, error: 'systemd-inhibit: Failed to inhibit: Access denied (org.freedesktop.login1.inhibit-block-sleep)', last_suspend: null } },
    sizes: ['desktop'],
    full: false,
  }),
]
