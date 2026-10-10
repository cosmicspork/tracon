// The review screen (/reviews/:id) in every state worth looking at: code
// reviews through each verdict and publication outcome, the 0.29.0 additions
// (unsent drafts on the node, the since-last-verdict view, the verdict bar's
// reasons, publication readiness and recovery, squashed commits, mirrored
// reviews), and the narrative report a review can also be.
//
// The review detail is sent pre-serialised with its own timestamps: the
// runner's stamping turns every small `*_ms` into "now + n", and that would
// make a check's `duration_ms` a date.

import { readFileSync } from 'node:fs'

const baseline = JSON.parse(readFileSync(new URL('../api.json', import.meta.url), 'utf8'))

// Generated from real file texts (before, first revision, second revision)
// with a unified differ, so the diff editor can rebuild each base exactly.
const D = {
 "full": "diff --git a/docs/config.md b/docs/config.md\nindex 5b2e01c..8f41d3a 100644\n--- a/docs/config.md\n+++ b/docs/config.md\n@@ -5,4 +5,5 @@\n | Key | Default | Meaning |\n |---|---|---|\n | `ceiling` | `4000000` | Tokens a channel may use in a day. |\n+| `per_minute` | `120` | Calls a channel may burst to in a minute. |\n | `listen` | `127.0.0.1:7421` | Where harnesses reach the gateway. |\ndiff --git a/src/gateway/limit.rs b/src/gateway/limit.rs\nnew file mode 100644\nindex 0000000..3f1a9c2\n--- /dev/null\n+++ b/src/gateway/limit.rs\n@@ -0,0 +1,43 @@\n+//! A token bucket per channel, refilled on read.\n+//!\n+//! The ceiling refuses a day's overuse; this smooths a burst inside it, so\n+//! one runaway loop cannot take a channel's whole allowance in a minute.\n+\n+use std::collections::HashMap;\n+use std::sync::Mutex;\n+use std::time::Instant;\n+\n+use crate::channel::ChannelName;\n+\n+pub struct Limiter {\n+    per_minute: u32,\n+    buckets: Mutex<HashMap<ChannelName, Bucket>>,\n+}\n+\n+struct Bucket {\n+    tokens: f64,\n+    at: Instant,\n+}\n+\n+impl Limiter {\n+    pub fn new(per_minute: u32) -> Self {\n+        Self { per_minute, buckets: Mutex::new(HashMap::new()) }\n+    }\n+\n+    /// Take one call from `channel`'s bucket; false when it is empty.\n+    pub fn take(&self, channel: &ChannelName) -> bool {\n+        let mut buckets = self.buckets.lock().unwrap();\n+        let now = Instant::now();\n+        let cap = self.per_minute as f64;\n+        let bucket = buckets.entry(channel.clone()).or_insert(Bucket { tokens: cap, at: now });\n+        let refill = now.duration_since(bucket.at).as_secs_f64() * cap / 60.0;\n+        // An idle channel refills to its capacity, never past it.\n+        bucket.tokens = (bucket.tokens + refill).min(cap);\n+        bucket.at = now;\n+        if bucket.tokens < 1.0 {\n+            return false;\n+        }\n+        bucket.tokens -= 1.0;\n+        true\n+    }\n+}\ndiff --git a/src/gateway/mod.rs b/src/gateway/mod.rs\nindex 1c7d0e4..a90b2f6 100644\n--- a/src/gateway/mod.rs\n+++ b/src/gateway/mod.rs\n@@ -1,23 +1,29 @@\n //! The gateway every brokered call passes through.\n \n mod ceiling;\n+mod limit;\n \n use crate::channel::ChannelName;\n use crate::error::GatewayError;\n \n pub use ceiling::Ceiling;\n+pub use limit::Limiter;\n \n pub struct Gateway {\n     ceiling: Ceiling,\n+    limiter: Limiter,\n }\n \n impl Gateway {\n-    pub fn new(ceiling: Ceiling) -> Self {\n-        Self { ceiling }\n+    pub fn new(ceiling: Ceiling, limiter: Limiter) -> Self {\n+        Self { ceiling, limiter }\n     }\n \n     /// Admit one call on `channel`, or say why not.\n     pub fn admit(&self, channel: &ChannelName, tokens: u64) -> Result<(), GatewayError> {\n+        if !self.limiter.take(channel) {\n+            return Err(GatewayError::Burst(channel.clone()));\n+        }\n         self.ceiling.check(channel, tokens)?;\n         Ok(())\n     }\ndiff --git a/tests/rate_limit.rs b/tests/rate_limit.rs\nnew file mode 100644\nindex 0000000..e4c18b7\n--- /dev/null\n+++ b/tests/rate_limit.rs\n@@ -0,0 +1,21 @@\n+use orbit::gateway::Limiter;\n+\n+#[test]\n+fn a_full_bucket_admits_its_capacity_then_refuses() {\n+    let limiter = Limiter::new(3);\n+    let channel = \"personal\".into();\n+    assert!(limiter.take(&channel));\n+    assert!(limiter.take(&channel));\n+    assert!(limiter.take(&channel));\n+    assert!(!limiter.take(&channel));\n+}\n+\n+#[test]\n+fn an_idle_bucket_refills_to_capacity_and_no_further() {\n+    let limiter = Limiter::new(2);\n+    let channel = \"personal\".into();\n+    limiter.advance(std::time::Duration::from_secs(3600));\n+    assert!(limiter.take(&channel));\n+    assert!(limiter.take(&channel));\n+    assert!(!limiter.take(&channel));\n+}\n",
 "fullAdded": 73,
 "fullRemoved": 2,
 "inter": "diff --git a/src/gateway/limit.rs b/src/gateway/limit.rs\nindex 72d0c3e..3f1a9c2 100644\n--- a/src/gateway/limit.rs\n+++ b/src/gateway/limit.rs\n@@ -31,7 +31,8 @@\n         let cap = self.per_minute as f64;\n         let bucket = buckets.entry(channel.clone()).or_insert(Bucket { tokens: cap, at: now });\n         let refill = now.duration_since(bucket.at).as_secs_f64() * cap / 60.0;\n-        bucket.tokens = bucket.tokens + refill;\n+        // An idle channel refills to its capacity, never past it.\n+        bucket.tokens = (bucket.tokens + refill).min(cap);\n         bucket.at = now;\n         if bucket.tokens < 1.0 {\n             return false;\ndiff --git a/tests/rate_limit.rs b/tests/rate_limit.rs\nindex 9a3be50..e4c18b7 100644\n--- a/tests/rate_limit.rs\n+++ b/tests/rate_limit.rs\n@@ -9,3 +9,13 @@\n     assert!(limiter.take(&channel));\n     assert!(!limiter.take(&channel));\n }\n+\n+#[test]\n+fn an_idle_bucket_refills_to_capacity_and_no_further() {\n+    let limiter = Limiter::new(2);\n+    let channel = \"personal\".into();\n+    limiter.advance(std::time::Duration::from_secs(3600));\n+    assert!(limiter.take(&channel));\n+    assert!(limiter.take(&channel));\n+    assert!(!limiter.take(&channel));\n+}\n",
 "interFiles": [
  {
   "path": "src/gateway/limit.rs",
   "added": 2,
   "removed": 1
  },
  {
   "path": "tests/rate_limit.rs",
   "added": 10,
   "removed": 0
  }
 ],
 "heads": {
  "docs/config.md": "# Configuration\n\n## `[gateway]`\n\n| Key | Default | Meaning |\n|---|---|---|\n| `ceiling` | `4000000` | Tokens a channel may use in a day. |\n| `per_minute` | `120` | Calls a channel may burst to in a minute. |\n| `listen` | `127.0.0.1:7421` | Where harnesses reach the gateway. |\n",
  "src/gateway/limit.rs": "//! A token bucket per channel, refilled on read.\n//!\n//! The ceiling refuses a day's overuse; this smooths a burst inside it, so\n//! one runaway loop cannot take a channel's whole allowance in a minute.\n\nuse std::collections::HashMap;\nuse std::sync::Mutex;\nuse std::time::Instant;\n\nuse crate::channel::ChannelName;\n\npub struct Limiter {\n    per_minute: u32,\n    buckets: Mutex<HashMap<ChannelName, Bucket>>,\n}\n\nstruct Bucket {\n    tokens: f64,\n    at: Instant,\n}\n\nimpl Limiter {\n    pub fn new(per_minute: u32) -> Self {\n        Self { per_minute, buckets: Mutex::new(HashMap::new()) }\n    }\n\n    /// Take one call from `channel`'s bucket; false when it is empty.\n    pub fn take(&self, channel: &ChannelName) -> bool {\n        let mut buckets = self.buckets.lock().unwrap();\n        let now = Instant::now();\n        let cap = self.per_minute as f64;\n        let bucket = buckets.entry(channel.clone()).or_insert(Bucket { tokens: cap, at: now });\n        let refill = now.duration_since(bucket.at).as_secs_f64() * cap / 60.0;\n        // An idle channel refills to its capacity, never past it.\n        bucket.tokens = (bucket.tokens + refill).min(cap);\n        bucket.at = now;\n        if bucket.tokens < 1.0 {\n            return false;\n        }\n        bucket.tokens -= 1.0;\n        true\n    }\n}\n",
  "src/gateway/mod.rs": "//! The gateway every brokered call passes through.\n\nmod ceiling;\nmod limit;\n\nuse crate::channel::ChannelName;\nuse crate::error::GatewayError;\n\npub use ceiling::Ceiling;\npub use limit::Limiter;\n\npub struct Gateway {\n    ceiling: Ceiling,\n    limiter: Limiter,\n}\n\nimpl Gateway {\n    pub fn new(ceiling: Ceiling, limiter: Limiter) -> Self {\n        Self { ceiling, limiter }\n    }\n\n    /// Admit one call on `channel`, or say why not.\n    pub fn admit(&self, channel: &ChannelName, tokens: u64) -> Result<(), GatewayError> {\n        if !self.limiter.take(channel) {\n            return Err(GatewayError::Burst(channel.clone()));\n        }\n        self.ceiling.check(channel, tokens)?;\n        Ok(())\n    }\n}\n",
  "tests/rate_limit.rs": "use orbit::gateway::Limiter;\n\n#[test]\nfn a_full_bucket_admits_its_capacity_then_refuses() {\n    let limiter = Limiter::new(3);\n    let channel = \"personal\".into();\n    assert!(limiter.take(&channel));\n    assert!(limiter.take(&channel));\n    assert!(limiter.take(&channel));\n    assert!(!limiter.take(&channel));\n}\n\n#[test]\nfn an_idle_bucket_refills_to_capacity_and_no_further() {\n    let limiter = Limiter::new(2);\n    let channel = \"personal\".into();\n    limiter.advance(std::time::Duration::from_secs(3600));\n    assert!(limiter.take(&channel));\n    assert!(limiter.take(&channel));\n    assert!(!limiter.take(&channel));\n}\n"
 }
}

const SELF = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
const PEER = '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1'
const HEAD = '7d3f9a1c5e2b8d40f6a9c3e1b7d5f2a8c4e0b6d9'
const PREV = 'c41e8b07a3d9f2e6b5c1a8d4e7f0b3c9a2d6e5f1'
const FIRST = '2a9e4c7b1f8d3a6e0c5b9f2d7a4e1c8b6f3d0a95'
const BASE_SHA = 'e8b1d4a7c0f3e6b9d2a5c8f1e4b7d0a3c6f9e2b5'
const TREE = '5c0a7e2d9b4f1c6e3a8d5b2f9c4e1a7d0b6f3e8c'
const ID = 'r-rate'

/** Small `*_ms` values are offsets from now, as in api.json; durations are not. */
function stamp(v, now) {
  if (Array.isArray(v)) return v.map((x) => stamp(x, now))
  if (v !== null && typeof v === 'object') {
    return Object.fromEntries(
      Object.entries(v).map(([k, val]) => [
        k,
        k.endsWith('_ms') && k !== 'duration_ms' && typeof val === 'number' && Math.abs(val) < 1e12
          ? now + val
          : stamp(val, now),
      ]),
    )
  }
  return v
}
/** A response body the runner passes through untouched. */
const raw = (body) => () => JSON.stringify(stamp(body, Date.now()))

const files = (paths) => JSON.stringify(paths.map((path, i) => ({ path, blob: `${(0x3f1a9c2 + i * 7919).toString(16)}e0b4d1c7a92f5e8b3d6c0a4f7e2b9d5c1a8` })))
const target = (o = {}) => JSON.stringify({ provider: 'github', project: 'example-org/orbit', base: 'main', branch: 'feat/rate-limits', ...o })

const REVIEWER = {
  ...baseline['/api/sessions'][1],
  id: 's-review',
  node_id: SELF,
  channel: 'personal',
  work_item_id: 'wi-2',
  model: 'opus',
  phase: 'review',
  review_id: ID,
  tokens_used: 48200,
  cost_usd: 0.41,
  branch: 'feat/rate-limits',
  created_ms: -1380000,
  updated_ms: -1260000,
}
const SESSIONS = [...baseline['/api/sessions'], REVIEWER]

const AI_APPROVE = JSON.stringify({
  verdict: 'approve',
  summary:
    'The bucket refills on read and is clamped at capacity; the gateway checks it before the daily ceiling, so a refused burst does not count against the day. Two small things worth a look.',
  findings: [
    { path: 'src/gateway/limit.rs', line: 31, severity: 'should', note: 'A poisoned mutex panics the gateway; `lock().unwrap_or_else(|e| e.into_inner())` keeps admitting.' },
    { path: 'docs/config.md', line: 8, severity: 'nit', note: 'Say that `per_minute = 0` disables the limiter, or refuse it at load.' },
  ],
  model: 'opus',
  session_id: 's-review',
  at_ms: 0,
})
const AI_CHANGES = JSON.stringify({
  verdict: 'request_changes',
  summary: 'The refill is unbounded: an idle channel accumulates tokens without limit and can then burst far past `per_minute`. The new test does not cover idling.',
  findings: [
    { path: 'src/gateway/limit.rs', line: 35, severity: 'blocking', note: 'Clamp `bucket.tokens` to `cap` after refilling.' },
    { path: 'tests/rate_limit.rs', severity: 'should', note: 'Add a test that idles the bucket and checks it refills to capacity and no further.' },
    { path: 'src/gateway/mod.rs', line: 24, severity: 'nit', note: '`GatewayError::Burst` could carry the retry-after the bucket already knows.' },
  ],
  model: 'opus',
  session_id: 's-review',
  at_ms: 0,
})

const CHECKS = JSON.stringify([
  { command: 'just check', ok: true, exit: 0, tail: 'ok', ms: 94000 },
  { command: 'cargo nextest run --workspace', ok: true, exit: 0, tail: '214 tests run: 214 passed', ms: 212000 },
])

function review(o = {}) {
  return {
    id: ID,
    session_id: 's-run',
    lane: null,
    node_id: SELF,
    channel: 'personal',
    kind: 'pr',
    title: 'Smooth bursts with a per-channel token bucket',
    body:
      'The daily ceiling only refuses a channel once it has spent its allowance, so one runaway loop can take the whole day in a minute.\n\nThis adds a token bucket per channel in front of the ceiling: `per_minute` calls refill continuously, and a call over it is refused with `GatewayError::Burst` before it counts against the day.\n\nVerified with `cargo nextest run --workspace` and by driving 500 calls through a local gateway: 120 admitted, the rest refused, none charged.',
    edited_title: null,
    edited_body: null,
    provider: 'github',
    target: target(),
    diff: D.full,
    files: files(['docs/config.md', 'src/gateway/limit.rs', 'src/gateway/mod.rs', 'tests/rate_limit.rs']),
    head_sha: HEAD,
    base_ref: 'main',
    added: D.fullAdded,
    removed: D.fullRemoved,
    state: 'claimed',
    verdict_reason: null,
    publish_result: null,
    claimed_ms: -20000,
    created_ms: -1500000,
    checks_json: CHECKS,
    review_session_id: 's-review',
    ai_verdict_json: AI_APPROVE,
    revision_patch: null,
    ...o,
  }
}

const run = (command, outcome, seconds, o = {}) => ({
  id: `run-${command.length}${seconds}`,
  candidate_id: 'cand-7d3f9a1c',
  session_id: 's-run',
  command,
  definition_json: JSON.stringify({ command, timeout_s: 900 }),
  definition_hash: 'b6e1f3a0c9d2',
  execution_image: 'ghcr.io/example-org/orbit-ci@sha256:4e9a1c7d0b3f6e2a8c5d1b9f4e7a0c3d6b2e8f5a1c4d7b0e3a6f9c2d5b8e1a4',
  inputs_json: JSON.stringify({ tree: TREE.slice(0, 12), lockfile: 'Cargo.lock@91c4e2', toolchain: '1.91.0' }),
  reuse_key: null,
  outcome,
  source_outcome: null,
  exit_code: outcome === 'passed' ? 0 : 1,
  log:
    outcome === 'passed'
      ? `$ ${command}\n   Compiling orbit v0.14.2 (/work)\n    Finished \`test\` profile [unoptimized + debuginfo] target(s) in 41.07s\n     Summary [  18.412s] 214 tests run: 214 passed, 0 skipped`
      : `$ ${command}\n   Compiling orbit v0.14.2 (/work)\nerror[E0599]: no method named \`advance\` found for struct \`Limiter\` in the current scope\n  --> tests/rate_limit.rs:18:13\n   |\n18 |     limiter.advance(std::time::Duration::from_secs(3600));\n   |             ^^^^^^^ method not found in \`Limiter\`\n\nerror: could not compile \`orbit\` (test "rate_limit") due to 1 previous error`,
  duration_ms: seconds * 1000,
  started_ms: -1490000,
  finished_ms: -1490000 + seconds * 1000,
  rerun_of: null,
  reused_from_id: null,
  metadata_json:
    outcome === 'failed'
      ? JSON.stringify({
          failures:
            'error[E0599]: no method named `advance` found for struct `Limiter` in the current scope\n--> tests/rate_limit.rs:18:13\nerror: could not compile `orbit` (test "rate_limit") due to 1 previous error',
        })
      : '{}',
  ...o,
})

function evidence(o = {}) {
  return {
    candidate: {
      id: 'cand-7d3f9a1c',
      head_sha: HEAD,
      tree_sha: TREE,
      channel: 'personal',
      owner_session_id: 's-run',
      source_kind: 'git',
      captured_ms: -1500000,
      capture_json: '{}',
    },
    checks: [run('just check', 'passed', 94), run('cargo nextest run --workspace', 'passed', 212)],
    revisions: [],
    decisions: [],
    demonstrations: [],
    ...o,
  }
}

const REQUIREMENTS = {
  id: 'wi-2',
  title: 'Add per-channel rate limits',
  body: 'The gateway refuses over-ceiling calls but nothing smooths bursts.\n\nA channel should be able to burst to a configured number of calls a minute; past that it is refused without spending its daily allowance.',
  hash: 'a1f4c9e2b7d0',
}

const SURROUNDING = [
  {
    path: 'src/gateway/ceiling.rs',
    start_line: 18,
    end_line: 31,
    text: '    /// Refuse `tokens` on `channel` once its day is spent.\n    pub fn check(&self, channel: &ChannelName, tokens: u64) -> Result<(), GatewayError> {\n        let used = self.used.get(channel).copied().unwrap_or(0);\n        if used + tokens > self.daily {\n            return Err(GatewayError::Ceiling(channel.clone()));\n        }\n        Ok(())\n    }',
  },
  {
    path: 'src/error.rs',
    start_line: 4,
    end_line: 16,
    text: '#[derive(Debug, thiserror::Error)]\npub enum GatewayError {\n    #[error("channel {0} has spent its daily ceiling")]\n    Ceiling(ChannelName),\n    #[error("channel {0} is calling faster than its limit")]\n    Burst(ChannelName),\n}',
  },
]

const IDENTITY = { name: 'Sam Operator', email: 'sam@example.com', login: 'sam-op', forge: 'github' }
const READY = {
  ready: true,
  provider: 'github',
  credential: 'GITHUB_TOKEN',
  project: 'example-org/orbit',
  problem: null,
  note: 'A GitHub token is bound to this channel. Whether it may push to example-org/orbit is only known when GitHub answers.',
  settings: `/settings?node=${SELF}#connections`,
}

const COMMITS = [
  { sha: 'a7c2e9f14b8d3062e5a1c7f9b4d2e8a0c3f6b1d9', subject: 'feat(gateway): add a token bucket per channel' },
  { sha: '3e8b0d5a9c2f7e14b6a0d3c8f5e2b9a7d1c4e0f6', subject: 'test(gateway): bucket admits its capacity then refuses' },
  { sha: HEAD, subject: 'fix(gateway): clamp the refill at capacity' },
]

function details(o = {}) {
  const { review: r, ...rest } = o
  return {
    review: review(r),
    revision: { id: 'rv-3', head_sha: HEAD, created_ms: -1500000 },
    intent: { forge: {}, commits: COMMITS },
    remote_owner: null,
    owner_detail: null,
    since_reviewed: null,
    stale: [],
    requirements: REQUIREMENTS,
    criteria: null,
    surrounding_code: SURROUNDING,
    evidence: evidence(),
    legacy_check_events: [],
    publications: [],
    publication: { readiness: READY, latest: null },
    shown_work: [],
    authorship: { identity: IDENTITY, misattributed: [], note: 'every commit is authored and committed as sam-op' },
    ...rest,
  }
}

const publication = (o = {}) => ({
  id: 'pub-5e1c0a',
  outcome: 'failed',
  note: null,
  url: null,
  attempts: 1,
  updated_ms: -240000,
  provider: 'github',
  project: 'example-org/orbit',
  branch: 'feat/rate-limits',
  remedy:
    'GitHub refused it, or the attempt stopped. A retry sends the same approved commit and description, and checks what GitHub holds before pushing again.',
  recoverable: true,
  refusal: null,
  ...o,
})

/** The file as submitted, for the diff editor. */
const file = (req) => ({ path: req.query.path, text: D.heads[req.query.path] ?? null })

function state(id, title, o = {}) {
  const rid = o.rid ?? ID
  return {
    id: `review-${id}`,
    area: 'review',
    route: `/reviews/${rid}`,
    title,
    note: o.note,
    since: o.since,
    sizes: o.sizes,
    full: o.full,
    // A full-page shot is taken from wherever the act left the page, and the
    // sticky rail and verdict bar are drawn where that scroll put them.
    act:
      o.act && o.full !== false
        ? async (page, ctx) => {
            await o.act(page, ctx)
            await page.evaluate(() => window.scrollTo(0, 0))
          }
        : o.act,
    api: {
      '/api/sessions': SESSIONS,
      [`/api/reviews/${rid}`]: o.detail === undefined ? raw(details()) : o.detail,
      [`/api/reviews/${rid}/draft`]: o.draft ?? { draft: null },
      [`/api/reviews/${rid}/file`]: file,
      'PUT /api/reviews/*/draft': { version: 2, updated_ms: 0 },
      'POST /api/reviews/*/release': null,
      ...o.api,
    },
  }
}

const settle = (page, ms = 1000) => page.waitForTimeout(ms)
const click = (page, name) => page.getByRole('button', { name }).first().click()

// --- larger diffs ---------------------------------------------------------

const CRATES = ['gateway', 'broker', 'store', 'mesh', 'session', 'review', 'corpus', 'http']
const MODS = ['mod', 'admit', 'state', 'events', 'route']
const LARGE_PATHS = CRATES.flatMap((c) => MODS.map((m) => `crates/${c}/src/${m}.rs`)).slice(0, 38)
LARGE_PATHS.push('crates/channel/src/lib.rs', 'crates/channel/src/name.rs', 'migrations/0042_channel_name.sql', 'CHANGELOG.md')
function largeDiff() {
  return LARGE_PATHS.map((path, i) => {
    const at = 12 + ((i * 17) % 140)
    const extra = i % 5 === 0
      ? `-    pub fn by_id(&self, id: ChannelId) -> Option<&Channel> {\n-        self.channels.iter().find(|c| c.id == id)\n+    pub fn by_name(&self, name: &ChannelName) -> Option<&Channel> {\n+        self.channels.iter().find(|c| &c.name == name)\n`
      : ''
    return (
      `diff --git a/${path} b/${path}\nindex ${(0x1a2b3c + i * 977).toString(16)}..${(0x9f8e7d - i * 613).toString(16)} 100644\n--- a/${path}\n+++ b/${path}\n` +
      `@@ -${at},9 +${at},9 @@\n use std::sync::Arc;\n \n-use crate::channel::ChannelId;\n+use crate::channel::ChannelName;\n use crate::store::Store;\n \n` +
      `-    channel: ChannelId,\n+    channel: ChannelName,\n${extra}     store: Arc<Store>,\n }\n`
    )
  }).join('')
}
const LARGE = largeDiff()

const KINDS_DIFF = `diff --git a/assets/logo.png b/assets/logo.png
index 3b18e51..9c2d7fa 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
diff --git a/docs/img/flow.svg b/docs/img/flow.svg
new file mode 100644
index 0000000..b4d29e1
Binary files /dev/null and b/docs/img/flow.svg differ
diff --git a/src/old_auth.rs b/src/auth/session.rs
similarity index 91%
rename from src/old_auth.rs
rename to src/auth/session.rs
index 6f0c2d1..e1a8b37 100644
--- a/src/old_auth.rs
+++ b/src/auth/session.rs
@@ -1,6 +1,6 @@
-//! Cookie sessions, before they moved under auth.
+//! Cookie sessions for the operator interface.

 use crate::store::Store;

 pub struct Session {
     pub id: String,
diff --git a/src/auth/mod.rs b/src/auth/mod.rs
index 0d4e7a2..5c9f1b8 100644
--- a/src/auth/mod.rs
+++ b/src/auth/mod.rs
@@ -1,3 +1,4 @@
 mod login;
+mod session;

 pub use login::login;
diff --git a/src/legacy/cookie.rs b/src/legacy/cookie.rs
deleted file mode 100644
index 7d1e0aa..0000000
--- a/src/legacy/cookie.rs
+++ /dev/null
@@ -1,9 +0,0 @@
-//! The pre-1.0 cookie format, read once to migrate.
-
-pub fn parse(raw: &str) -> Option<(String, u64)> {
-    let (id, rest) = raw.split_once('.')?;
-    let expires = rest.parse().ok()?;
-    Some((id.to_string(), expires))
-}
-
--- read only by the 0.9 migration, gone since 0.12
diff --git a/scripts/release.sh b/scripts/release.sh
old mode 100644
new mode 100755
`
const KINDS_PATHS = ['assets/logo.png', 'docs/img/flow.svg', 'src/auth/session.rs', 'src/auth/mod.rs', 'src/legacy/cookie.rs', 'scripts/release.sh']

// --- long everything ---------------------------------------------------------

const LONG_BRANCH = 'feat/gateway-per-channel-token-bucket-rate-limiting-with-configurable-burst-and-refill-and-retry-after-headers'
const LONG_PATH = 'crates/orbit-gateway-rate-limiting/src/implementation/token_bucket/refill_strategies/continuous_refill_clamped_at_capacity.rs'
const LONG_DIFF = `diff --git a/${LONG_PATH} b/${LONG_PATH}
new file mode 100644
index 0000000..4be19c0
--- /dev/null
+++ b/${LONG_PATH}
@@ -0,0 +1,6 @@
+//! A refill strategy that is continuous and clamped at capacity, so an idle channel never accumulates more than one bucket of burst headroom regardless of how long it has been idle, which is the property the gateway's documentation promises and the test below checks.
+
+pub const FIXTURE_SIGNATURE: &str = "MEUCIQDx3k9vZ2h0LXRvLWJlLWxvbmctYW5kLXVuYnJva2VuLWFuZC1rZWVwcy1nb2luZy1hbmQtZ29pbmctYW5kLWdvaW5nLWZvci1hLXdoaWxlLWxvbmdlci10aGFuLWFueS1zY3JlZW4=";
+
+pub fn clamp(tokens: f64, refill: f64, cap: f64) -> f64 { (tokens + refill).min(cap) }
+
`

// --- criteria ------------------------------------------------------------------

const link = (index, value, outcome, o = {}) => ({ index, provenance: 'decided', standard: 'agreed', kind: 'check', value, refs: [], outcome, ...o })
const CRITERIA = {
  work_item_id: 'wi-2',
  channel: 'personal',
  brief_slug: 'brief-rate-limits',
  hash: 'c0ffee12ab34',
  candidate: { id: 'cand-7d3f9a1c', head_sha: HEAD, captured_ms: -1500000 },
  criteria: [
    {
      key: 'c1',
      text: 'A channel can burst to `per_minute` calls, and the next is refused.',
      provenance: 'decided',
      standard: 'agreed',
      refs: [],
      links: [link(0, 'cargo nextest run -E test(a_full_bucket)', 'passed')],
      judgement: { id: 'j-1', verdict: 'met', note: 'read the test and drove 500 calls locally', candidate_id: 'cand-7d3f9a1c', criterion_text: 'A channel can burst to `per_minute` calls, and the next is refused.', judged_ms: -600000 },
      coverage: 'judged_met',
    },
    {
      key: 'c2',
      text: 'A refused burst does not count against the daily ceiling.',
      provenance: 'decided',
      standard: 'agreed',
      refs: [],
      links: [link(0, 'cargo nextest run -E test(refused_burst_is_free)', 'passed')],
      coverage: 'checks_pass',
    },
    {
      key: 'c3',
      text: 'An idle channel refills to capacity and no further.',
      provenance: 'decided',
      standard: 'agreed',
      refs: [],
      links: [link(0, 'cargo nextest run -E test(an_idle_bucket)', 'failed')],
      earlier_judgement: { id: 'j-0', verdict: 'not_met', note: 'refill was unbounded', candidate_id: 'cand-c41e8b07', criterion_text: 'An idle channel refills to capacity and no further.', judged_ms: -86400000 },
      coverage: 'failing',
    },
    {
      key: 'c4',
      text: 'The refusal tells the harness when to retry.',
      provenance: 'inferred',
      standard: 'proposed',
      refs: [],
      links: [link(0, 'cargo nextest run -E test(retry_after)', undefined, { provenance: 'inferred', standard: 'proposed', unresolved: 'only proposed by the agent; nobody agreed it' })],
      coverage: 'only_proposed',
    },
    {
      key: 'c5',
      text: 'Operators can see a channel\'s current burst headroom.',
      provenance: 'observed',
      standard: 'agreed',
      refs: [],
      links: [],
      coverage: 'nothing_points_at_it',
    },
  ],
  gaps: {
    uncovered: ['c4', 'c5'],
    unjudged: ['c2', 'c3', 'c4', 'c5'],
    assumptions: [],
    questions: ['Should a burst refusal be logged as a policy decision?'],
    orphaned_judgements: [],
  },
  summary: '5 criteria · 1 judged met · 1 failing · 1 only the agent\'s proposal · 1 nothing points at it',
}

// --- shown work ------------------------------------------------------------------

const SHOWN = [
  {
    id: 'sw-1',
    channel: 'personal',
    session_id: 's-run',
    lane: null,
    review_id: ID,
    head_sha: HEAD,
    title: 'Driving 500 calls through a local gateway',
    format: 'markdown',
    markdown:
      'Ran `orbit-load --calls 500 --channel personal` against a gateway with `per_minute = 120`.\n\n| | calls |\n|---|---|\n| admitted | 120 |\n| refused (burst) | 380 |\n| charged to the day | 120 |\n\nThe daily counter moved by 120 only.',
    document_id: null,
    document_slug: null,
    document_hash: null,
    files: [],
    created_ms: -1420000,
    stale: false,
    stale_reason: null,
    checks: [
      { command: 'just check', outcome: 'passed', source_outcome: null },
      { command: 'cargo nextest run --workspace', outcome: 'reused', source_outcome: 'passed' },
    ],
  },
  {
    id: 'sw-2',
    channel: 'personal',
    session_id: 's-run',
    lane: null,
    review_id: ID,
    head_sha: PREV,
    title: 'Burst headroom chart',
    format: 'files',
    markdown: 'The bucket level over a minute of load, before the refill was clamped.',
    document_id: 'doc-77',
    document_slug: 'show-burst-headroom',
    document_hash: 'f00dbabe',
    files: [
      { path: 'headroom.svg', size_bytes: 18342 },
      { path: 'load.csv', size_bytes: 2210331 },
    ],
    created_ms: -5400000,
    stale: true,
    stale_reason: 'the candidate moved past c41e8b07 since this was shown',
    checks: [{ command: 'cargo nextest run --workspace', outcome: 'failed', source_outcome: null }],
  },
]

// --- narrative reports -------------------------------------------------------------

const REPORT_HASH = 'b2c7e19f04a6d83c5e1f7a9b0d4c2e8f6a3b5d1c9e7f0a2b4d6c8e1f3a5b7d9c'
function report(o = {}) {
  return {
    ...review({
      id: 'r-report',
      kind: 'report',
      title: 'Why the nightly import has been slow since Tuesday',
      body:
        '## Summary\n\nThe nightly import went from about 4 minutes to **38 minutes** on Tuesday. The cause is the new `invoices_by_vendor` index: every insert now updates it, and the importer inserts row by row inside one transaction.\n\n## What I looked at\n\n- The import log for the last 10 nights (attached to the session).\n- `EXPLAIN QUERY PLAN` for the insert, before and after the migration.\n- The migration that added the index: https://forge.example.net/example-org/ledger/-/blob/main/migrations/0031_invoices_by_vendor_index_for_the_vendor_statement_report.sql\n\n## What I suggest\n\n1. Drop and recreate the index around the import, or\n2. batch the inserts 500 at a time.\n\nI have not changed anything. Option 2 is smaller and keeps the index available to the vendor report during the import.',
      provider: 'none',
      target: JSON.stringify({ kind: 'narrative_report', session_id: null, lane: 'claude-code · ledger' }),
      diff: '',
      files: '[]',
      head_sha: REPORT_HASH,
      base_ref: 'none',
      added: 0,
      removed: 0,
      session_id: null,
      lane: 'claude-code · ledger',
      state: 'claimed',
      checks_json: null,
      review_session_id: null,
      ai_verdict_json: null,
      created_ms: -3300000,
    }),
    ...o,
  }
}
const reportDetail = (o = {}) => ({
  review: report(o),
  revision: null,
  stale: [],
  requirements: null,
  criteria: null,
  surrounding_code: [],
  evidence: null,
  legacy_check_events: [],
  publications: [],
  shown_work: [],
})

// --- states ----------------------------------------------------------------------

export default [
  state('pending', 'Code review, awaiting your verdict', {
    note: 'The typical screen: evidence, the review model approving with findings, readiness note, commits, files and diff, and the verdict bar stuck to the bottom.',
    since: '#397',
    detail: raw(details()),
  }),

  state('loading', 'Loading', {
    note: 'The review request never answers.',
    detail: () => new Promise(() => {}),
  }),

  state('not-found', 'No such review', {
    detail: { status: 404, body: { error: { code: 404, message: 'no such review' } } },
  }),

  state('server-error', 'The node failed reading it', {
    note: 'A 500 is shown the same way as a missing review.',
    detail: { status: 500, body: { error: { code: 500, message: 'database is locked' } } },
  }),

  state('tiny', 'One-line change from an external harness, review model still reading', {
    note: 'Submitted by an agent you run yourself: no session, no node checks, the review model has not answered yet.',
    detail: raw(
      details({
        review: {
          session_id: null,
          lane: 'claude-code · ~/src/orbit',
          title: 'Fix a typo in the gateway error',
          body: 'Says "ceiling", not "cieling".',
          target: target({ branch: 'fix/ceiling-typo', worktree: '/home/op/src/.worktrees/orbit-ceiling-typo' }),
          diff: 'diff --git a/src/error.rs b/src/error.rs\nindex 2b7c0e1..8e4d9a3 100644\n--- a/src/error.rs\n+++ b/src/error.rs\n@@ -6,7 +6,7 @@ use crate::channel::ChannelName;\n #[derive(Debug, thiserror::Error)]\n pub enum GatewayError {\n-    #[error("channel {0} has spent its daily cieling")]\n+    #[error("channel {0} has spent its daily ceiling")]\n     Ceiling(ChannelName),\n     #[error("channel {0} is calling faster than its limit")]\n     Burst(ChannelName),\n',
          files: files(['src/error.rs']),
          added: 1,
          removed: 1,
          checks_json: null,
          ai_verdict_json: null,
          created_ms: -45000,
        },
        evidence: evidence({ checks: [] }),
        intent: { forge: {}, commits: [{ sha: HEAD, subject: 'fix(gateway): spell ceiling' }] },
        requirements: null,
        surrounding_code: [],
      }),
    ),
  }),

  state('large', 'A 42-file mechanical rename', {
    note: 'Many files in the list and a long diff in its scroll box; phone shows the per-file accordion.',
    detail: raw(
      details({
        review: {
          title: 'Name channels by name, not by numeric id',
          body: 'Mechanical: `ChannelId` becomes `ChannelName` everywhere, with a migration that rewrites the stored ids. No behaviour change; `just check` and the full test suite pass.',
          diff: LARGE,
          files: files(LARGE_PATHS),
          added: 102,
          removed: 102,
        },
        intent: { forge: {}, commits: [{ sha: HEAD, subject: 'refactor: name channels by name, not id' }] },
      }),
    ),
  }),

  state('file-kinds', 'Binary, new binary, renamed, deleted and mode-only files', {
    note: 'How the diff and the phone file list read files with no hunks, a rename, and a deletion whose content starts with `--`.',
    detail: raw(
      details({
        review: {
          title: 'Move cookie sessions under auth and drop the legacy parser',
          body: 'Moves `src/old_auth.rs` to `src/auth/session.rs`, deletes the pre-1.0 cookie parser (only the 0.9 migration read it), refreshes the logo, adds the flow diagram, and makes the release script executable.',
          diff: KINDS_DIFF,
          files: files(KINDS_PATHS),
          added: 2,
          removed: 10,
        },
        intent: { forge: {}, commits: [{ sha: HEAD, subject: 'refactor(auth): move sessions under auth' }] },
      }),
    ),
  }),

  state('long-content', 'Long title, branch, paths, body and unbroken strings', {
    note: 'Overflow: the header title, the Publishes line with a long branch, finding paths, the files list, and a diff line with no break.',
    detail: raw(
      details({
        review: {
          title:
            'Smooth bursts with a per-channel token bucket in front of the daily ceiling, refilled continuously and clamped at capacity, with a retry-after hint on refusal and documentation for every new key',
          body:
            'See https://forge.example.net/example-org/orbit/issues/1287#issuecomment-99887766554433221100-and-the-rest-of-a-very-long-anchor-that-never-breaks\n\n' +
            'The daily ceiling only refuses a channel once it has spent its allowance. '.repeat(8),
          target: target({ project: 'example-org/orbit-platform-gateway-and-broker-monorepo', branch: LONG_BRANCH }),
          diff: LONG_DIFF + D.full,
          files: files([LONG_PATH, 'docs/config.md', 'src/gateway/limit.rs', 'src/gateway/mod.rs', 'tests/rate_limit.rs']),
          ai_verdict_json: JSON.stringify({
            ...JSON.parse(AI_CHANGES),
            findings: [
              { path: LONG_PATH, line: 1, severity: 'nit', note: 'This doc comment is one 230-character line; wrap it.' },
              ...JSON.parse(AI_CHANGES).findings,
            ],
          }),
        },
        requirements: { ...REQUIREMENTS, title: 'Add per-channel rate limits so that one runaway agent loop cannot spend a channel\'s entire daily allowance within a minute', body: REQUIREMENTS.body + '\n\nReference: https://docs.example.net/orbit/gateway/rate-limits/design-notes-and-alternatives-considered-including-leaky-bucket-and-sliding-window' },
        surrounding_code: [{ ...SURROUNDING[0], path: LONG_PATH }],
        intent: { forge: {}, commits: [{ sha: HEAD, subject: 'feat(gateway): smooth bursts with a per-channel token bucket in front of the daily ceiling, refilled continuously and clamped at capacity' }] },
      }),
    ),
  }),

  state('update-pr', 'Updates an open pull request, rewriting its history', {
    note: 'Existing change: summary is read-only, the rewrite chip, the agent asked to comment, and the review model requesting changes with a blocking finding.',
    detail: raw(
      details({
        review: {
          target: target({ change: { number: 412, url: 'https://github.com/example-org/orbit/pull/412' } }),
          ai_verdict_json: AI_CHANGES,
        },
        intent: {
          forge: { comment: 'Rebased onto main and clamped the refill; the new test idles the bucket for an hour.' },
          lease: PREV,
          rewrite: true,
          commits: COMMITS,
        },
      }),
    ),
  }),

  state('update-pr-push-only', 'Updates an open pull request, nothing but the push', {
    note: 'With neither description nor comment: the "Approving only pushes" note.',
    detail: raw(
      details({
        review: { target: target({ change: { number: 412, url: 'https://github.com/example-org/orbit/pull/412' } }) },
        intent: { forge: {}, commits: COMMITS },
      }),
    ),
  }),

  state('squash', 'Ships as one commit of the reviewed tree', {
    note: 'The agent\'s commits struck through (dimmed), the proposed message, and the branch name; phone shows them read-only.',
    since: '#394',
    detail: raw(details({ intent: { forge: { draft: true }, squash_onto: BASE_SHA, commits: COMMITS } })),
  }),

  state('squash-edited', 'Commit message and branch edited before approving', {
    note: 'The Publishes line follows the edited branch, "Edited." appears, and the draft is saved to the node.',
    since: '#394',
    sizes: ['desktop'],
    detail: raw(details({ intent: { forge: { draft: true }, squash_onto: BASE_SHA, commits: COMMITS } })),
    act: async (page) => {
      const msg = page.getByLabel('commit message')
      await msg.fill('feat(gateway): smooth bursts with a per-channel token bucket\n\nA call over `per_minute` is refused before it counts against the day.')
      await page.locator('label.branch input').fill('feat/gateway-burst-limit')
      await settle(page)
    },
  }),

  state('since', 'Resubmission: what changed since your last verdict', {
    note: 'Opens on the interdiff, with each piece of feedback and the files that answered it; the tab bar switches to the full change.',
    since: '#396',
    detail: raw(
      details({
        since_reviewed: {
          revision_id: 'rv-2',
          head_sha: PREV,
          created_ms: -5400000,
          diff: D.inter,
          files: D.interFiles,
          unavailable: null,
          responses: [
            { revision_id: 'rv-1', decision: 'revise', source: 'operator', reason: 'Document `per_minute` in docs/config.md.', sent_edit: false, decided_ms: -9000000, answered_by: 'rv-2', files: [{ path: 'docs/config.md', added: 1, removed: 0 }] },
            { revision_id: 'rv-2', decision: 'revise', source: 'operator', reason: 'An idle channel refills without limit. Clamp it and add a test that idles the bucket.', sent_edit: true, decided_ms: -3600000, answered_by: 'rv-3', files: D.interFiles },
          ],
        },
      }),
    ),
  }),

  state('since-full', 'Resubmission, switched to the full change', {
    since: '#396',
    detail: raw(
      details({
        since_reviewed: {
          revision_id: 'rv-2',
          head_sha: PREV,
          created_ms: -5400000,
          diff: D.inter,
          files: D.interFiles,
          unavailable: null,
          responses: [
            { revision_id: 'rv-2', decision: 'revise', source: 'operator', reason: 'An idle channel refills without limit. Clamp it and add a test.', sent_edit: false, decided_ms: -3600000, answered_by: 'rv-3', files: D.interFiles },
          ],
        },
      }),
    ),
    act: async (page) => {
      await page.getByRole('tab', { name: /Full change/ }).click()
    },
  }),

  state('since-unavailable', 'Resubmission whose worktree is gone', {
    note: 'No interdiff and no tabs: the reason, and responses whose files can no longer be read; one decision is from an older node (legacy source).',
    since: '#396',
    detail: raw(
      details({
        since_reviewed: {
          revision_id: 'rv-2',
          head_sha: PREV,
          created_ms: -5400000,
          diff: null,
          files: null,
          unavailable: 'the worktree this review was captured from is gone',
          responses: [
            { revision_id: 'rv-1', decision: 'revise', source: 'legacy_unknown', reason: null, sent_edit: false, decided_ms: -90000000, answered_by: 'rv-2', files: null },
            { revision_id: 'rv-2', decision: 'reject', source: 'authority', reason: 'Policy: changes to the gateway need a linked work item.', sent_edit: false, decided_ms: -3600000, answered_by: 'rv-3', files: null },
          ],
        },
      }),
    ),
  }),

  state('since-prose-only', 'Resubmission with the same commit, new prose', {
    note: 'Empty interdiff: "only the prose was resubmitted", and the full diff below.',
    since: '#396',
    detail: raw(
      details({
        since_reviewed: {
          revision_id: 'rv-2',
          head_sha: HEAD,
          created_ms: -5400000,
          diff: '',
          files: [],
          unavailable: 'the commit is unchanged; only the prose was resubmitted',
          responses: [
            { revision_id: 'rv-2', decision: 'revise', source: 'operator', reason: 'Say how it was verified in the description.', sent_edit: false, decided_ms: -3600000, answered_by: 'rv-3', files: [] },
          ],
        },
      }),
    ),
  }),

  state('revising', 'Changes requested, waiting on the agent', {
    note: 'The revising banner with your reason; the verdict bar is still offered.',
    detail: raw(details({ review: { state: 'revising', verdict_reason: 'Clamp the refill at capacity and add a test that idles the bucket.' } })),
  }),

  state('compose-revise', 'Request changes composer, nothing written yet', {
    note: 'Send is disabled and says why beside it.',
    since: '#397',
    act: async (page) => {
      await click(page, /^Request changes…$/)
    },
  }),

  state('compose-reject', 'Reject composer with a reason', {
    since: '#397',
    act: async (page) => {
      await click(page, /^Reject…$/)
      await page.locator('.decide textarea').fill('This duplicates the limiter in the broker; extend that one instead.')
      await settle(page)
    },
  }),

  state('stale', 'Files changed in the worktree since submit', {
    note: 'Red header, files marked, approve disabled with the reason under the bar.',
    since: '#397',
    detail: raw(details({ stale: ['src/gateway/limit.rs', 'tests/rate_limit.rs'] })),
  }),

  state('publishing', 'Publication in flight or interrupted', {
    note: 'Every verdict disabled with the reconcile reason.',
    since: '#397',
    detail: raw(
      details({
        review: { state: 'publishing' },
        publication: { readiness: READY, latest: publication({ outcome: 'in_progress', remedy: '', recoverable: false }) },
      }),
    ),
  }),

  state('publish-failed', 'Publication failed, retry offered', {
    note: 'The failed banner, the remedy, and Retry publication.',
    since: '#381',
    detail: raw(
      details({
        publication: {
          readiness: READY,
          latest: publication({ note: 'GitHub refused the push: protected branch hook declined (required status check "ci" is expected)' }),
        },
      }),
    ),
  }),

  state('publish-retry-refused', 'Retry publication refused', {
    note: 'After pressing Retry: the refusal under the recovery row.',
    since: '#381',
    detail: raw(details({ publication: { readiness: READY, latest: publication({ note: 'GitHub answered 502 Bad Gateway' }) } })),
    api: {
      'POST /api/reviews/r-rate/publication/recover': { status: 502, body: { error: { code: 502, message: 'GitHub answered 502 Bad Gateway while reading the branch; nothing was pushed' } } },
    },
    act: async (page) => {
      await click(page, 'Retry publication')
      await settle(page, 600)
    },
  }),

  state('publish-uncertain', 'Publication outcome unknown, retry refused', {
    note: 'An uncertain outcome that may not be retried, with the refusal in place of the button.',
    since: '#381',
    detail: raw(
      details({
        publication: {
          readiness: READY,
          latest: publication({
            outcome: 'uncertain',
            attempts: 2,
            remedy: 'Tracon could not tell what GitHub holds. A retry looks at GitHub first and only does what has not already happened.',
            recoverable: false,
            refusal: 'a newer revision was submitted since this was approved; approving it is a new decision',
          }),
        },
      }),
    ),
  }),

  state('publish-not-attempted', 'Publication not attempted: no token bound', {
    note: 'Not ready and not attempted together: the readiness line, the retry, and Approve disabled.',
    since: '#381',
    detail: raw(
      details({
        publication: {
          readiness: { ...READY, ready: false, problem: 'the GitHub token (GITHUB_TOKEN) is not bound to the personal channel', note: undefined },
          latest: publication({
            outcome: 'not_attempted',
            note: 'the GitHub token (GITHUB_TOKEN) is not bound to the personal channel',
            remedy: 'Nothing reached GitHub. Fix what stopped it, then retry the same approved publication.',
          }),
        },
      }),
    ),
  }),

  state('not-ready', 'Cannot publish from this node yet', {
    note: 'No token stored: Approve is disabled, and the bar says why with a Settings link.',
    since: '#381',
    detail: raw(
      details({
        publication: {
          readiness: { ready: false, provider: 'github', credential: 'GITHUB_TOKEN', project: 'example-org/orbit', problem: 'no GitHub token (GITHUB_TOKEN) is stored on this node', settings: `/settings?node=${SELF}#connections` },
          latest: null,
        },
      }),
    ),
  }),

  state('misattributed', 'Commits not authored as the forge account', {
    note: 'The authorship banner naming each commit.',
    since: '#382',
    detail: raw(
      details({
        authorship: {
          identity: IDENTITY,
          misattributed: [
            { sha: COMMITS[0].sha, author: 'Claude', author_email: 'noreply@anthropic.com', committer_email: 'noreply@anthropic.com' },
            { sha: COMMITS[1].sha, author: 'agent', author_email: 'agent@orbit-rate.local', committer_email: 'agent@orbit-rate.local' },
          ],
          note: '2 commits are not authored as sam-op',
        },
      }),
    ),
  }),

  state('mirrored', 'Mirrored from another node, detail read from it', {
    note: 'Held by work-pod and read from it: the banner, owner evidence, and the forge section and editor that defer to the owner.',
    since: '#392',
    detail: raw(
      details({
        review: { node_id: PEER, channel: 'work', session_id: 's-plan', review_session_id: null, ai_verdict_json: null, target: target({ project: 'platform/orbit', branch: 'feat/queue-metrics' }) },
        remote_owner: PEER,
        owner_detail: { state: 'fetched' },
        requirements: { ...REQUIREMENTS, id: 'wi-6' },
        publication: null,
        authorship: null,
        since_reviewed: null,
        intent: { forge: {}, squash_onto: BASE_SHA, commits: COMMITS.slice(0, 2) },
        criteria: { ...CRITERIA, channel: 'work' },
      }),
    ),
  }),

  state('mirrored-unreachable', 'Mirrored, owner unreachable', {
    note: 'The red banner; evidence, intent and criteria absent.',
    since: '#392',
    detail: raw(
      details({
        review: { node_id: PEER, channel: 'work', session_id: 's-plan', review_session_id: null, ai_verdict_json: null, target: target({ project: 'platform/orbit', branch: 'feat/queue-metrics' }) },
        remote_owner: PEER,
        owner_detail: { state: 'unreachable', reason: 'the node that holds it is unreachable; reload when it returns' },
        publication: null,
        authorship: null,
        intent: null,
        evidence: null,
        surrounding_code: [],
        requirements: null,
      }),
    ),
  }),

  state('mirrored-moved', 'Mirrored, owner holds a newer revision', {
    since: '#392',
    detail: raw(
      details({
        review: { node_id: PEER, channel: 'work', session_id: 's-plan', review_session_id: null, ai_verdict_json: null, target: target({ project: 'platform/orbit', branch: 'feat/queue-metrics' }) },
        remote_owner: PEER,
        owner_detail: { state: 'moved', reason: 'the node that holds it has a newer revision than this node has mirrored; reload once it arrives' },
        publication: null,
        authorship: null,
        intent: null,
        evidence: null,
        surrounding_code: [],
      }),
    ),
  }),

  state('verdict-bar-in-view', 'The verdict bar mid-page, publication not ready', {
    note: 'Viewport only, scrolled to the middle: what the sticky bar shows while Approve is disabled because no token is bound.',
    since: '#397',
    full: false,
    detail: raw(
      details({
        publication: {
          readiness: { ready: false, provider: 'github', credential: 'GITHUB_TOKEN', project: 'example-org/orbit', problem: 'no GitHub token (GITHUB_TOKEN) is stored on this node', settings: `/settings?node=${SELF}#connections` },
          latest: null,
        },
      }),
    ),
    act: async (page) => {
      await page.getByText('On the forge').first().evaluate((el) => el.scrollIntoView({ block: 'start' }))
    },
  }),

  state('draft-conflict-in-view', 'Draft conflict while writing in the bar', {
    note: 'Viewport only: writing a reason in the sticky composer mid-page after another device saved.',
    since: '#395',
    full: false,
    api: {
      'PUT /api/reviews/*/draft': {
        status: 409,
        body: {
          error: { code: 409, message: 'this draft was changed on another device since you loaded it' },
          draft: { review_id: ID, revision_id: 'rv-3', draft: { reason: 'Clamp the refill and add an idle test.' }, version: 4, updated_ms: -5000 },
        },
      },
    },
    act: async (page) => {
      await page.getByText('On the forge').first().evaluate((el) => el.scrollIntoView({ block: 'start' }))
      await click(page, /^Request changes…$/)
      await page.locator('.decide textarea').fill('The refill needs clamping at capacity.')
      await settle(page, 1200)
    },
  }),

  state('phone-bar-in-view', 'The verdict bar on a phone, as it sits over the tab bar', {
    note: 'Viewport only, phone: all three verdicts should be reachable above the bottom navigation.',
    since: '#397',
    sizes: ['phone'],
    full: false,
    act: async (page) => {
      await page.getByText('Title and body').first().evaluate((el) => el.scrollIntoView({ block: 'start' }))
    },
  }),

  state('phone-compose-in-view', 'Writing a reason on a phone', {
    note: 'Viewport only, phone: the composer, its send button and the reason it is disabled.',
    since: '#397',
    sizes: ['phone'],
    full: false,
    act: async (page) => {
      await page.getByText('Title and body').first().evaluate((el) => el.scrollIntoView({ block: 'start' }))
      await click(page, /^Request changes…$/)
    },
  }),

  state('phone-refused-in-view', 'A refused approval on a phone, mid-page', {
    note: 'Viewport only, phone: the refusal shows in the bar, beside the verdict that drew it, not below the diff.',
    sizes: ['phone'],
    full: false,
    api: {
      'POST /api/reviews/*/verdict': {
        status: 409,
        body: { error: { code: 409, message: 'this review moved to a new revision while it was being decided; reload it and decide again' } },
      },
    },
    act: async (page) => {
      await page.getByText('Title and body').first().evaluate((el) => el.scrollIntoView({ block: 'start' }))
      await click(page, 'Approve and publish')
      await settle(page, 800)
    },
  }),

  state('phone-draft-unsaved-in-view', 'An unsaved draft while writing a reason on a phone', {
    note: 'Viewport only, phone: "draft not saved" shows in the bar under the composer.',
    sizes: ['phone'],
    full: false,
    api: { 'PUT /api/reviews/*/draft': { status: 500, body: { error: { code: 500, message: 'database is locked' } } } },
    act: async (page) => {
      await page.getByText('Title and body').first().evaluate((el) => el.scrollIntoView({ block: 'start' }))
      await click(page, /^Request changes…$/)
      await page.locator('.decide textarea').fill('The refill needs clamping at capacity.')
      await settle(page, 1200)
    },
  }),

  state('draft-restored', 'Unsent words restored from the node', {
    note: 'A draft saved on another device against an earlier revision comes back over the agent\'s text: "draft saved · written against an earlier revision".',
    since: '#395',
    detail: raw(details({ intent: { forge: {}, squash_onto: BASE_SHA, commits: COMMITS } })),
    draft: {
      draft: {
        review_id: ID,
        revision_id: 'rv-2',
        draft: {
          reason: '',
          title: 'Smooth bursts with a per-channel token bucket',
          body: 'Adds `[gateway] per_minute`. A call over it is refused before it counts against the day.',
          describe: true,
          descTitle: 'feat(gateway): per-channel burst limit',
          descBody: 'Closes #1287.\n\nA token bucket per channel sits in front of the daily ceiling.',
          commenting: true,
          comment: 'Thanks — verified locally with 500 calls.',
          message: 'feat(gateway): per-channel burst limit',
          branch: 'feat/gateway-burst-limit',
        },
        version: 3,
        updated_ms: -600000,
      },
    },
  }),

  state('draft-conflict', 'Draft changed on another device', {
    note: 'Writing a reason here after another device saved: the conflict banner with both choices.',
    since: '#395',
    api: {
      'PUT /api/reviews/*/draft': {
        status: 409,
        body: {
          error: { code: 409, message: 'this draft was changed on another device since you loaded it' },
          draft: { review_id: ID, revision_id: 'rv-3', draft: { reason: 'Clamp the refill and add an idle test.' }, version: 4, updated_ms: -5000 },
        },
      },
    },
    act: async (page) => {
      await click(page, /^Request changes…$/)
      await page.locator('.decide textarea').fill('The refill needs clamping at capacity.')
      await settle(page, 1200)
    },
  }),

  state('draft-unsaved', 'Draft could not be saved', {
    note: 'The node answered 500 to the save: "draft not saved".',
    since: '#395',
    api: { 'PUT /api/reviews/*/draft': { status: 500, body: { error: { code: 500, message: 'database is locked' } } } },
    act: async (page) => {
      await click(page, /^Request changes…$/)
      await page.locator('.decide textarea').fill('The refill needs clamping at capacity.')
      await settle(page, 1200)
    },
  }),

  state('superseded', 'Approve refused: the review moved on', {
    note: 'Approving after the agent resubmitted: the refusal, with the draft resumed.',
    since: '#395',
    api: {
      'POST /api/reviews/*/verdict': {
        status: 409,
        body: { error: { code: 409, message: 'this review moved to a new revision while it was being decided; the same commit may have been resubmitted with different requirements or prose, so reload it and decide again' } },
      },
    },
    act: async (page) => {
      await click(page, 'Approve and publish')
      await settle(page, 800)
    },
  }),

  state('approved', 'Approved and published', {
    note: 'A review that has been approved and published, opened again from its session: decided, linking the pull request, no verdicts.',
    detail: raw(
      details({
        review: { state: 'approved', publish_result: 'https://github.com/example-org/orbit/pull/418' },
        publication: { readiness: READY, latest: publication({ outcome: 'published', url: 'https://github.com/example-org/orbit/pull/418', remedy: '', recoverable: false }) },
      }),
    ),
  }),

  state('rejected', 'Rejected', {
    note: 'A rejected review opened again: decided, with its reason and no verdicts.',
    detail: raw(details({ review: { state: 'rejected', verdict_reason: 'The ceiling already covers this; a burst limit is not wanted.' } })),
  }),

  state('edit-diff', 'Editing the diff in place', {
    note: 'The CodeMirror merge view for each file, as submitted (viewport only: CodeMirror renders only the lines in view).',
    sizes: ['desktop'],
    full: false,
    act: async (page) => {
      await click(page, 'Edit the diff')
      await page.locator('.cm-editor').first().waitFor({ timeout: 8000 })
      await page.locator('.filehead').nth(2).evaluate((el) => el.scrollIntoView({ block: 'start' }))
      await settle(page, 500)
    },
  }),

  state('edit-diff-edited', 'An edit, ready to go back with a request for changes', {
    note: 'The edited chip, the count beside Discard, and the composer labelled for edits.',
    sizes: ['desktop'],
    full: false,
    act: async (page) => {
      await click(page, 'Edit the diff')
      const line = page.locator('.cm-content').first().locator('.cm-line').first()
      await line.waitFor({ timeout: 8000 })
      await line.click()
      await page.keyboard.press('End')
      await page.keyboard.type(' for orbit')
      await click(page, /^Send edits and request changes…$/)
      await settle(page, 900)
      await page.locator('.filehead').first().evaluate((el) => el.scrollIntoView({ block: 'start' }))
      await settle(page, 200)
    },
  }),

  state('edit-diff-nothing', 'Editing a diff with nothing editable', {
    note: 'Every file binary, new without a base, or deleted: the "nothing here can be edited" banner.',
    sizes: ['desktop'],
    detail: raw(details({ review: { diff: KINDS_DIFF, files: files(KINDS_PATHS), added: 2, removed: 10 } })),
    api: { '/api/reviews/r-rate/file': (req) => ({ path: req.query.path, text: null }) },
    act: async (page) => {
      await click(page, 'Edit the diff')
      await settle(page, 800)
    },
  }),

  state('criteria', 'Acceptance criteria on a GitLab merge request', {
    note: 'Criteria in each coverage state with a judge form open, an open question; GitLab says "merge request"; failing check in the evidence.',
    detail: raw(
      details({
        review: {
          provider: 'gitlab',
          kind: 'mr',
          target: target({ provider: 'gitlab', project: 'platform/orbit' }),
          checks_json: JSON.stringify([{ command: 'just check', ok: true, exit: 0, tail: 'ok', ms: 94000 }]),
        },
        criteria: CRITERIA,
        evidence: evidence({
          checks: [
            run('just check', 'passed', 94),
            run('cargo nextest run --workspace', 'failed', 61),
            run('just e2e', 'reused', 0, { source_outcome: 'passed', reused_from_id: 'run-9c1e7a04', duration_ms: null }),
          ],
          demonstrations: [
            { id: 'dm-1', candidate_id: 'cand-7d3f9a1c', channel: 'personal', document_id: 'doc-3', document_slug: 'ref-rate-limit-demo', document_hash: 'aa11', label: 'Load test notes', created_ms: -1200000, stale: false },
            { id: 'dm-2', candidate_id: 'cand-7d3f9a1c', channel: 'personal', document_id: 'doc-4', document_slug: 'ref-headroom-dashboard', document_hash: 'bb22', label: 'Headroom dashboard mock', created_ms: -9200000, stale: true },
          ],
        }),
        publication: { readiness: { ...READY, provider: 'gitlab', credential: 'GITLAB_TOKEN', project: 'platform/orbit', note: 'A GitLab token is bound to this channel. Whether it may push to platform/orbit is only known when GitLab answers.' }, latest: null },
      }),
    ),
    act: async (page) => {
      await page.getByRole('button', { name: 'Judge it', exact: true }).first().click()
      await page.getByPlaceholder('what you looked at').fill('ran the idle test by hand')
    },
  }),

  state('shown-work', 'What the agent showed of its work', {
    note: 'An account with a table and checks, and a stale file listing with a failed check.',
    detail: raw(details({ shown_work: SHOWN })),
  }),

  state('legacy', 'A review from before candidate capture', {
    note: 'No revision, evidence, intent or publication: the evidence-missing banner.',
    detail: raw({
      review: review({ review_session_id: null, ai_verdict_json: null, checks_json: null }),
      revision: null,
      stale: [],
      requirements: null,
      criteria: null,
      surrounding_code: [],
      evidence: null,
      legacy_check_events: [],
    }),
  }),

  // Narrative reports render through ReportReview.
  state('report', 'Narrative report awaiting acknowledgement', {
    rid: 'r-report',
    note: 'The report body rendered as Markdown (headings, a list, inline code), with a long unbroken URL, and the decision form.',
    detail: raw(reportDetail()),
  }),

  state('report-revising', 'Narrative report, changes requested', {
    rid: 'r-report',
    detail: raw(reportDetail({ state: 'revising', verdict_reason: 'Measure option 2 before suggesting it.' })),
  }),

  state('report-acknowledged', 'Narrative report, acknowledged', {
    rid: 'r-report',
    detail: raw(reportDetail({ state: 'acknowledged', verdict_reason: 'Go with batching.', session_id: 's-run', lane: null })),
  }),

  state('report-refused', 'Narrative report, decision refused', {
    rid: 'r-report',
    note: 'A note typed and Request changes pressed after the report was resubmitted.',
    api: {
      'POST /api/reviews/*/verdict': { status: 409, body: { error: { code: 409, message: 'this report was resubmitted while it was being decided; reload it and decide again' } } },
    },
    detail: raw(reportDetail()),
    act: async (page) => {
      await page.locator('#report-note').fill('Measure option 2 before suggesting it.')
      await click(page, 'Request changes')
      await settle(page, 600)
    },
  }),

  state('report-unavailable', 'Narrative report in a state this node cannot read', {
    rid: 'r-report',
    note: 'A mirrored report whose state is not one ReportReview knows: the receipt-unavailable banner.',
    detail: raw(reportDetail({ state: 'gone', node_id: PEER })),
  }),
]
