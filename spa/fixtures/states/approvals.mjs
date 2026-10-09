// The approval page (/approvals/:id) and the memory batch page
// (/promotions/:id). Approval fixtures follow GET /api/approvals/{id}
// (node/src/http/api.rs get_approval): the row, the parsed arguments, the
// tool's input schema and its presentation (node/src/mcp/schema.rs). The
// node's serde_json has no preserve_order, so schemas and arguments arrive
// with their keys sorted; so do these, which is why `body` comes before
// `number` and `repo` in a pr_comment form.

const NODE = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
const SESSION = '0199f2c4-5e1a-7b3d-9c8e-2f4a6b8d0e13'

const sorted = (v) => {
  if (Array.isArray(v)) return v.map(sorted)
  if (v !== null && typeof v === 'object') {
    return Object.fromEntries(Object.keys(v).sort().map((k) => [k, sorted(v[k])]))
  }
  return v
}

const CARD_OPTIONS = [
  { option_id: 'allow_once', name: 'Allow once', kind: 'allow_once' },
  { option_id: 'reject_once', name: 'Reject', kind: 'reject_once' },
  { option_id: 'request_changes', name: 'Request changes', kind: 'request_changes' },
]
const EGRESS_OPTIONS = [
  { option_id: 'allow_once', name: 'Allow once', kind: 'allow_once' },
  { option_id: 'allow_session', name: 'For this session', kind: 'allow_session' },
  { option_id: 'allow_repo', name: "Save to the repository's egress", kind: 'allow_repo' },
  { option_id: 'reject_once', name: 'Reject', kind: 'reject_once' },
]
const IDENTIFYING = ['key', 'project', 'repo', 'number', 'iid']

// Tool input schemas as node/src/mcp/{github,jira,egress,setup,docs}.rs define them.
const repo = { type: 'string', description: 'owner/name' }
const SCHEMAS = {
  pr_comment: {
    type: 'object',
    properties: { repo, number: { type: 'integer' }, body: { type: 'string' } },
    required: ['repo', 'number', 'body'],
  },
  pr_reply: {
    type: 'object',
    properties: {
      repo,
      number: { type: 'integer' },
      thread_id: { type: 'string', description: "The thread's id from pr_threads." },
      body: { type: 'string', description: 'The reply. May be omitted when only resolving.' },
      resolve: { type: 'boolean', description: 'Mark the thread resolved after replying.' },
    },
    required: ['repo', 'number', 'thread_id'],
  },
  pr_merge: {
    type: 'object',
    properties: {
      repo,
      number: { type: 'integer' },
      head_sha: { type: 'string', description: 'The reviewed pull request head SHA.' },
      method: { type: 'string', enum: ['merge', 'squash', 'rebase'] },
      operation_id: { type: 'string', description: 'Stable idempotency id for this merge.' },
    },
    required: ['repo', 'number', 'head_sha', 'operation_id'],
  },
  run_rerun: {
    type: 'object',
    properties: { repo, run_id: { type: 'integer' } },
    required: ['repo', 'run_id'],
  },
  issue_transition: {
    type: 'object',
    properties: { key: { type: 'string' }, transition_id: { type: 'string' }, operation_id: { type: 'string' } },
    required: ['key', 'transition_id', 'operation_id'],
  },
  issue_comment: {
    type: 'object',
    properties: { key: { type: 'string' }, body: { type: 'string' } },
    required: ['key', 'body'],
  },
  issue_create: {
    type: 'object',
    properties: {
      project: { type: 'string', description: 'Project key, e.g. WRK.' },
      type: { type: 'string', description: 'Issue type name, e.g. Task.' },
      summary: { type: 'string' },
      description: { type: 'string' },
      priority: { type: 'string' },
      labels: { type: 'array', items: { type: 'string' } },
      parent: { type: 'string', description: 'Parent issue key.' },
    },
    required: ['project', 'type', 'summary'],
  },
  request_egress: {
    type: 'object',
    properties: {
      host: { type: 'string', description: 'The host name, such as `pypi.org`.' },
      why: { type: 'string', description: 'What you need it for, in a sentence the operator will read.' },
      wait_secs: { type: 'integer', description: 'How long to block, up to 45. 0 asks and returns at once.' },
    },
    required: ['host'],
  },
  repo_setup_propose: {
    type: 'object',
    properties: {
      image: {
        type: 'string',
        description:
          "A toolchain image pinned by digest (`name@sha256:…`). Name this or `dockerfile`, not both; neither means the node's harness image.",
      },
      dockerfile: {
        type: 'string',
        description:
          "The repository's dev Dockerfile, as a path inside it (commonly `.devcontainer/Dockerfile`). The node builds it from the default branch.",
      },
      context: {
        type: 'string',
        description:
          "The build context for `dockerfile`, inside the repository; `.` is the root. Left out, it is the Dockerfile's directory.",
      },
      checks: {
        type: 'array',
        items: { type: 'string' },
        description:
          'The commands that must pass before a change is reviewed. Each runs in a fresh copy of the prepared tree, with no network and the dependency cache read-only.',
      },
      prepare: {
        type: 'array',
        items: { type: 'string' },
        description:
          'Commands run once before the checks, with the dependency cache writable and `egress` reachable: installs and fetches (`cargo fetch --locked`, `bun install --frozen-lockfile`). Prefer flags that skip install scripts.',
      },
      egress: {
        type: 'array',
        items: { type: 'string' },
        description: 'What preparation may reach: a preset (`crates`, `npm`, `pypi`, `packagist`, `github`) or a host name.',
      },
      timeout_secs: { type: 'integer', minimum: 1, description: 'How long preparation, and each check, may take.' },
      why: {
        type: 'string',
        description: 'What the trial showed, and anything you could not settle, in a few sentences the operator will read.',
      },
      repo: { type: 'string', description: 'Filled in by the node: the repository this session works in.' },
    },
    required: ['checks'],
  },
}

// (format, prose fields, locked fields beyond IDENTIFYING), as PRESENTATIONS has them.
const PRESENT = {
  pr_comment: ['markdown', ['body'], []],
  pr_reply: ['markdown', ['body'], ['thread_id']],
  issue_comment: ['jira_wiki', ['body'], []],
  issue_create: ['jira_wiki', ['description'], []],
  request_egress: ['markdown', [], ['host']],
  repo_setup_propose: ['markdown', ['why'], []],
}

/** mcp::summarize: a prose call is titled by target and first line; anything else is `name {json}`. */
function summarize(tool, args) {
  const first = (k) => (typeof args[k] === 'string' ? args[k].split('\n').map((l) => l.trim()).find(Boolean) : undefined)
  const withSaid = (head, said) => (said ? `${head}: ${said}` : head)
  let s
  switch (tool) {
    case 'pr_comment':
    case 'pr_reply':
      s = withSaid(`${tool} ${args.repo}#${args.number}`, first('body'))
      break
    case 'issue_comment':
      s = withSaid(`${tool} ${args.key}`, first('body'))
      break
    case 'issue_create':
      s = withSaid(`${tool} ${args.project} ${args.type}`, args.summary)
      break
    default:
      s = `${tool} ${JSON.stringify(sorted(args))}`
  }
  return s.length > 400 ? `${s.slice(0, 400)}…` : s
}

/**
 * One GET /api/approvals/{id} body. `row` overrides ApprovalRow fields;
 * `extra` overrides the top-level details.
 */
function approval(id, tool, args, row = {}, extra = {}) {
  const [format, prose, locked] = PRESENT[tool] ?? ['markdown', [], []]
  const a = sorted(args)
  const edited = row.edited_arguments !== undefined ? row.edited_arguments : null
  const result = row.result !== undefined ? row.result : null
  return {
    approval: {
      id,
      channel: 'personal',
      session_id: SESSION,
      node_id: NODE,
      lane: null,
      tool,
      arguments: JSON.stringify(a),
      request_key: `${tool}:${id.slice(-6)}`,
      title: summarize(tool, args),
      state: 'pending',
      answer_option_id: null,
      reason: null,
      operator_note: null,
      claimed_ms: -2000,
      created_ms: -190000,
      decided_ms: null,
      finished_ms: null,
      expires_ms: 1610000,
      ...row,
      edited_arguments: edited === null ? null : JSON.stringify(sorted(edited)),
      result: result === null ? null : JSON.stringify(result),
    },
    arguments: a,
    edited_arguments: edited === null ? null : sorted(edited),
    result,
    input_schema: SCHEMAS[tool] ? sorted(SCHEMAS[tool]) : null,
    format,
    prose_fields: prose,
    locked_fields: [...IDENTIFYING, ...locked],
    lane: row.lane ?? null,
    claimed_before_ms: null,
    options: tool === 'request_egress' ? EGRESS_OPTIONS : CARD_OPTIONS,
    ...extra,
  }
}

const COMMENT_BODY = `## Review of the retry change

The backoff now caps at **30s**, which matches what the gateway tolerates. Two things before this merges:

1. \`RetryPolicy::next_delay\` adds jitter *after* the cap, so a delay can exceed 30s by up to 20%.
2. The new test only covers the first three attempts; the cap is never reached in it.

\`\`\`rust
let delay = base.saturating_mul(2u32.pow(attempt)).min(cap);
let delay = delay + jitter(delay / 5); // can land above cap
\`\`\`

> CI is green on \`8c4be91\`; the flaky \`gateway::reconnect\` test passed on the rerun.

Otherwise this looks ready.`

const prComment = (id, row, extra) =>
  approval(id, 'pr_comment', { repo: 'acme/orbit', number: 412, body: COMMENT_BODY }, row, extra)

/** A state for one approval id, with its GET and release answered. */
function at(id, body, rest = {}) {
  const { api, ...more } = rest
  return {
    area: 'approvals',
    route: `/approvals/${id}`,
    api: {
      [`GET /api/approvals/${id}`]: body,
      [`POST /api/approvals/${id}/release`]: { status: 204, body: '' },
      ...(api ?? {}),
    },
    ...more,
  }
}

const textareaOf = (page, key) => page.locator('label.field', { has: page.locator('.name', { hasText: key }) }).locator('textarea')
const inputOf = (page, key) =>
  page.locator('label.field', { has: page.locator('.name', { hasText: new RegExp(`^${key}\\b`) }) }).locator('input')

// ---- long content -------------------------------------------------------

const LONG_URL =
  'https://ci.example.test/acme/orbit-platform-gateway/actions/runs/11893472205/job/33104882917?pr=4127&check_suite_focus=true&very_long_query_parameter_that_does_not_break=abcdefghijklmnopqrstuvwxyz0123456789'
const LONG_BODY = `Re-ran the full matrix against ${'`'}0f3c9a1e7b2d4c6f8a0e1b3d5f7a9c2e4b6d8f0a${'`'} and the failure is the same one we saw last week: the integration job cannot reach the fixture registry from inside the boundary, so every test that pulls an image times out at exactly 120s.

Log: ${LONG_URL}

| job | attempt | result | duration |
|---|---|---|---|
| unit (ubuntu-24.04) | 1 | passed | 4m 12s |
| integration (ubuntu-24.04) | 1 | timed out | 20m 00s |
| integration (ubuntu-24.04) | 2 | timed out | 20m 00s |
| e2e / chromium / desktop-and-phone-viewports | 1 | cancelled | 0m 41s |

\`\`\`
error[E0277]: the trait bound \`GatewayHandle: Send\` is not satisfied in \`impl Future<Output = Result<(), GatewayError>>\` at crates/gateway/src/reconnect.rs:218:31 (required by a bound in tokio::task::spawn)
\`\`\`

AVeryLongIdentifierWithoutAnyBreakOpportunitiesThatAnAgentMightPasteFromAStackTraceOrABase64BlobLikeThisOne_aGVsbG8gd29ybGQgdGhpcyBpcyBhIHZlcnkgbG9uZyBzdHJpbmc=

### What I would do next

- Pin the fixture registry host in the repository's egress instead of asking for it per session.
- Split the integration job so the image pulls happen in preparation, where the network is open.
- Leave the e2e job alone; its cancellation is a consequence of the integration failure, not a separate problem.`

const longComment = approval(
  '0199f2d8-long-4c1a-8e2b-7d9f0a3c5e61',
  'pr_comment',
  {
    repo: 'acme-platform-infrastructure/orbit-platform-gateway-and-control-plane',
    number: 4127,
    body: `Integration failures on this branch are environmental, not caused by the change itself, and here is the evidence from three reruns across the full matrix with timings.\n\n${LONG_BODY}`,
  },
  {},
)

// ---- setup proposal (#391) ----------------------------------------------

const proposeArgs = {
  repo: 'acme/orbit',
  dockerfile: '.devcontainer/Dockerfile',
  context: '.',
  prepare: ['cargo fetch --locked', 'bun install --frozen-lockfile --ignore-scripts --cwd spa'],
  checks: ['cargo fmt --check', 'cargo clippy --workspace --all-targets -- -D warnings', 'cargo nextest run --workspace', 'bun run --cwd spa check'],
  egress: ['crates', 'npm', 'static.rust-lang.org'],
  timeout_secs: 1800,
  why: `Tried the draft twice in a fresh container.

- **First trial failed** in preparation: \`bun install\` ran the \`esbuild\` postinstall, which reached \`registry.npmjs.org\` for a platform binary. Adding \`--ignore-scripts\` fixed it; the vendored binary in \`spa/node_modules/.bin\` is enough for the checks.
- **Second trial passed**: preparation 6m 40s, the slowest check (\`cargo nextest run\`) 11m 02s, so 1800s leaves headroom.

Not settled: \`static.rust-lang.org\` is only needed when \`rust-toolchain.toml\` changes channel. I left it in rather than have a toolchain bump fail preparation, but you may prefer to open it per session instead.`,
}

// ---- states ------------------------------------------------------------

const ID = {
  comment: '0199f2d1-8a4e-7c3b-b1d2-4e6f8a0c2e47',
  reply: '0199f2d2-3b5c-7d1e-a0f2-6c8e0a2b4d61',
  merge: '0199f2d3-9c1d-7e4f-82a3-1b5d7f9b1c35',
  rerun: '0199f2d4-0d2e-7f50-93b4-2c6e8a0c2d46',
  transition: '0199f2d5-1e3f-7061-a4c5-3d7f9b1d3e57',
  create: '0199f2d6-2f40-7172-b5d6-4e80ac2e4f68',
  egress: '0199f2d7-3051-7283-86e7-5f91bd3f5079',
  service: '0199f2d9-5273-74a5-a809-71b3df517291',
  propose: '0199f2da-6384-75b6-b91a-82c4e06283a2',
  unnamed: '0199f2db-7495-76c7-8a2b-93d5f17394b3',
}

const reply = approval(
  ID.reply,
  'pr_reply',
  {
    repo: 'acme/orbit',
    number: 412,
    thread_id: 'PRRT_kwDOK7xq0c5r2VbM',
    body: 'Fixed in 3e1f0a2: jitter is now applied before the cap, and the new `caps_at_thirty_seconds` test drives ten attempts.',
    resolve: true,
  },
  { session_id: null, lane: 'claude-code-laptop', channel: 'personal' },
)

const merge = approval(ID.merge, 'pr_merge', {
  repo: 'acme/orbit',
  number: 412,
  head_sha: '3e1f0a2c9b8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f',
  method: 'squash',
  operation_id: 'merge-acme-orbit-412-3e1f0a2',
})

const rerun = approval(ID.rerun, 'run_rerun', { repo: 'acme/orbit', run_id: 11893472205 })

const transition = approval(ID.transition, 'issue_transition', {
  key: 'WRK-1874',
  transition_id: '31',
  operation_id: 'transition-WRK-1874-31-0199f2d5',
})

const create = approval(ID.create, 'issue_create', {
  project: 'WRK',
  type: 'Task',
  summary: 'Rotate the gateway signing key before the March expiry',
  priority: 'High',
  labels: ['security', 'gateway', 'q1'],
  description: `h2. Why
The gateway's signing key expires on *14 March*. After that, every session token it issued is refused and running sessions stop at their next tool call.

h2. What to do
# Generate a new key with {{tracon keys rotate --kind gateway}}.
# Publish the new public half to the mesh before retiring the old one.
# Keep the old key verifying for 24 hours.

h3. Risk
* A peer that misses the publish refuses new tokens until it next syncs.
* {color:red}Do not{color} delete the old key file until the overlap ends.

{code}
tracon keys list --kind gateway
{code}`,
})

const egress = approval(ID.egress, 'request_egress', {
  host: 'registry.npmjs.org',
  why: 'bun install needs the esbuild platform binary, which the lockfile does not vendor',
}, {
  title: `reach registry.npmjs.org from session ${SESSION.slice(0, 8)} · bun install needs the esbuild platform binary, which the lockfile does not vendor`,
})

const service = approval(ID.service, 'service_start', { name: 'postgres', wait_secs: 30 })

const propose = approval(ID.propose, 'repo_setup_propose', proposeArgs)

const unnamed = approval(ID.unnamed, 'issue_comment', {
  key: 'WRK-1874',
  body: 'Key rotation is scheduled for *Tuesday 09:00*; the overlap window is 24 hours.\n\n* new key id: {{gw-2026-03}}\n* old key retires: Wednesday 09:00',
  visibility: { type: 'role', value: 'Developers' },
  notify: false,
})

/** A settled approval: the same call after its answer. */
const settled = (base, row) => ({
  ...base,
  ...approval(base.approval.id, base.approval.tool, base.arguments, {
    title: base.approval.title,
    claimed_ms: null,
    decided_ms: -60000,
    ...row,
  }),
})

export default [
  // ---- pending, one per kind of call ---------------------------------
  at(ID.comment, prComment(ID.comment), {
    id: 'approvals-pr-comment',
    title: 'PR comment waiting: rendered Markdown, locked target',
    note: 'Prose renders as a document above the form; repo and number are locked chips; three buttons plus the reason input in the decide row. Form order is alphabetical (body, number, repo) because the node serialises the schema sorted.',
  }),
  at(ID.comment, prComment(ID.comment), {
    id: 'approvals-pr-comment-raw',
    title: 'PR comment, raw source toggled',
    note: 'The "Raw source" link swaps the rendered body for its Markdown source.',
    act: async (page) => {
      await page.getByRole('button', { name: 'Raw source' }).click()
    },
  }),
  at(ID.comment, prComment(ID.comment), {
    id: 'approvals-pr-comment-edited',
    title: 'PR comment edited before allowing',
    note: '"What you changed" diff appears, the document above shows the draft, and Allow reads "Allow with edits".',
    act: async (page) => {
      const ta = textareaOf(page, 'body')
      await ta.fill(COMMENT_BODY.replace('Otherwise this looks ready.', 'Otherwise this looks ready once the jitter is applied before the cap.'))
    },
  }),
  at(ID.comment, prComment(ID.comment), {
    id: 'approvals-reason-typed',
    title: 'Reason typed: Reject and Request changes enabled',
    note: 'Request changes and Reject are disabled until the reason input has text.',
    act: async (page) => {
      await page.locator('.decide input').fill('Hold this until the jitter fix lands; I will comment myself.')
    },
  }),
  at(ID.reply, reply, {
    id: 'approvals-pr-reply-external',
    title: 'PR reply from an external lane, with resolve',
    note: 'No session: "Asked by" shows the lane. thread_id is locked; resolve is a checkbox.',
  }),
  at(ID.merge, merge, {
    id: 'approvals-pr-merge',
    title: 'PR merge: enum select, JSON-ish title',
    note: 'A call with no prose is titled `pr_merge {json}` by the node. method renders as a select.',
  }),
  at(ID.rerun, rerun, {
    id: 'approvals-run-rerun',
    since: '#399',
    title: 'GitHub Actions rerun (new gated tool)',
    note: 'run_rerun is new in 0.29.0 and asked every time. Only repo (locked) and run_id; the page is mostly header.',
  }),
  at(ID.rerun, rerun, {
    id: 'approvals-validation-integer',
    since: '#399',
    title: 'Edit fails the schema: run_id is not an integer',
    note: 'The field turns red with "must be an integer" and Allow is disabled; the change diff still shows.',
    act: async (page) => {
      await inputOf(page, 'run_id').fill('11893472205-latest')
    },
  }),
  at(ID.transition, transition, {
    id: 'approvals-issue-transition',
    since: '#401',
    title: 'Jira transition by id',
    note: 'issue_transition takes the id issue_transitions (new in #401) lists; the card shows only the opaque id "31", never the transition name.',
  }),
  at(ID.create, create, {
    id: 'approvals-issue-create-jira',
    title: 'Jira issue create: wiki markup rendered, labels as tags',
    note: 'format jira_wiki: headings, numbered list, {{code}}, colour and {code} rendered. labels is a comma-separated input; project is locked.',
  }),
  at(ID.unnamed, unnamed, {
    id: 'approvals-unnamed-args',
    title: 'Arguments the schema does not name',
    note: '"not in the schema" chips for visibility (an object) and notify, shown read-only below the form.',
  }),
  at(ID.egress, egress, {
    id: 'approvals-egress',
    title: 'Egress request: four answers',
    note: 'request_egress has Allow once / For this session / Save to the repository\'s egress / Reject, and no Request changes. host is locked; why is a one-line text input.',
  }),
  at(ID.service, service, {
    id: 'approvals-service-start',
    since: '#389',
    title: 'Service sidecar start (no schema known)',
    note: 'service_start is not in the node\'s schema registry, so input_schema is null and the form is read off the arguments: name and wait_secs, both optional, nothing locked.',
  }),
  at(ID.propose, propose, {
    id: 'approvals-setup-propose',
    since: '#391',
    title: "Repository entry proposed from a session",
    note: 'repo_setup_propose: why renders as a document, checks/prepare/egress as comma-separated inputs (commas inside a command would split it), long field descriptions, and a 400-char JSON title.',
  }),
  at(ID.propose, propose, {
    id: 'approvals-setup-propose-edited',
    since: '#391',
    title: 'Repository entry with an edited check list',
    note: 'Tags edit shows as a JSON-ish array diff in "What you changed".',
    act: async (page) => {
      await inputOf(page, 'checks').fill('cargo fmt --check, cargo clippy --workspace --all-targets -- -D warnings, cargo nextest run --workspace')
      await inputOf(page, 'timeout_secs').fill('2400')
    },
  }),
  at(longComment.approval.id, longComment, {
    id: 'approvals-long-content',
    title: 'Long title, long repo, table, wide code and unbroken strings',
    note: 'Check the header wraps, the table and code block scroll inside the document, and the unbroken URL/base64 do not push the page wider (phone especially).',
  }),

  // ---- the node's answers --------------------------------------------
  at(ID.comment, prComment(ID.comment, {}, { claimed_before_ms: -95000 }), {
    id: 'approvals-claimed-elsewhere',
    title: 'Already open in another tab',
    note: 'Dim banner under the session row: whichever answers first decides it.',
  }),
  at(ID.comment, prComment(ID.comment), {
    id: 'approvals-refused-422',
    title: "Node refused the answer field by field (422)",
    note: 'Per-field errors under body and the locked number, a general problem as its own banner, and "refused · the node refused this answer as sent".',
    api: {
      'POST /api/permissions/*/answer': {
        status: 422,
        body: {
          error: {
            code: 422,
            message: 'body: must be at most 65536 characters; the arguments changed since the agent asked',
            fields: [
              { field: 'body', message: 'must be at most 65536 characters' },
              { field: 'number', message: 'names what this call acts on and cannot be edited; reject the call and ask for a new one instead' },
              { field: '', message: 'the arguments changed since the agent asked; reload the approval' },
            ],
          },
        },
      },
    },
    act: async (page) => {
      await page.getByRole('button', { name: /^Allow/ }).click()
    },
  }),
  at(ID.comment, prComment(ID.comment), {
    id: 'approvals-request-changes-422',
    title: 'Request changes refused: notes required',
    note: 'The reason input turns red and the node\'s "notes: say what should change" shows under the decide row.',
    api: {
      'POST /api/permissions/*/answer': {
        status: 422,
        body: { error: { code: 422, message: 'notes: say what should change', fields: [{ field: 'notes', message: 'say what should change' }] } },
      },
    },
    act: async (page) => {
      await page.locator('.decide input').fill('   .')
      await page.getByRole('button', { name: 'Request changes' }).click()
    },
  }),
  {
    // Allow races another device: the POST is a 409 and the reload shows what
    // the other answer did. The page's fetch is wrapped so the reload can be
    // told apart from the first read (the overrides see the query).
    ...at(ID.comment, (req) =>
      req.query.after
        ? settled(prComment(ID.comment), { state: 'succeeded', answer_option_id: 'allow_once', finished_ms: -3000, result: { url: 'https://github.com/acme/orbit/pull/412#issuecomment-2417730011' } })
        : prComment(ID.comment),
    ),
    id: 'approvals-conflict-409',
    title: 'Answered elsewhere first (409, then reloaded)',
    note: 'The node says "this approval is no longer waiting"; the page reloads to the outcome and keeps the refusal banner.',
    api: {
      [`GET /api/approvals/${ID.comment}`]: (req) =>
        req.query.after
          ? settled(prComment(ID.comment), { state: 'succeeded', answer_option_id: 'allow_once', finished_ms: -3000, result: { url: 'https://github.com/acme/orbit/pull/412#issuecomment-2417730011' } })
          : prComment(ID.comment),
      [`POST /api/approvals/${ID.comment}/release`]: { status: 204, body: '' },
      'POST /api/permissions/*/answer': { status: 409, body: { error: { code: 409, message: 'this approval is no longer waiting' } } },
    },
    init: () => {
      const orig = window.fetch
      let answered = false
      window.fetch = (input, init) => {
        const url = typeof input === 'string' ? input : input.url
        if (/\/answer$/.test(url)) answered = true
        else if (answered && /^\/api\/approvals\/[^/]+$/.test(url)) input = `${url}?after=1`
        return orig(input, init)
      }
    },
    act: async (page) => {
      await page.getByRole('button', { name: /^Allow/ }).click()
    },
  },
  at(ID.comment, prComment(ID.comment, { expires_ms: -30000, created_ms: -1830000 }), {
    id: 'approvals-lapsed',
    title: 'Expired while open (still pending on the node)',
    note: 'Head reads "expired · nothing ran", the form and decide row are gone, and an "expired unanswered" banner shows.',
  }),

  // ---- decided -------------------------------------------------------
  at(
    ID.comment,
    settled(prComment(ID.comment), {
      state: 'succeeded',
      answer_option_id: 'allow_once',
      finished_ms: -55000,
      operator_note: 'Softened the last line.',
      edited_arguments: {
        repo: 'acme/orbit',
        number: 412,
        body: COMMENT_BODY.replace('Otherwise this looks ready.', 'Otherwise this looks ready once the jitter is applied before the cap.'),
      },
      result: { url: 'https://github.com/acme/orbit/pull/412#issuecomment-2417730011', id: 2417730011 },
    }),
    {
      id: 'approvals-succeeded-edited',
      title: 'Allowed with edits and ran',
      note: 'Green banner with the note, the document shows what ran, "What the operator changed" diff, and the agent\'s original arguments expanded.',
      act: async (page) => {
        await page.locator('details.request summary').click()
      },
    },
  ),
  at(ID.merge, settled(merge, { state: 'running', answer_option_id: 'allow_once', finished_ms: null }), {
    id: 'approvals-running',
    title: 'Allowed, running now',
    note: 'Green "Allowed · running now" banner; collapsed original arguments.',
  }),
  at(
    ID.rerun,
    settled(rerun, {
      state: 'failed',
      answer_option_id: 'allow_once',
      finished_ms: -40000,
      reason:
        'GitHub refused the rerun (HTTP 403): Resource not accessible by personal access token. The token for channel personal lacks the actions:write permission on acme/orbit; add it in Settings → Credentials, then ask the agent to call run_rerun again.',
    }),
    {
      id: 'approvals-failed',
      since: '#399',
      title: 'Allowed but failed, long reason',
      note: 'Red banner with a long reason; the reason is in italics inside the banner.',
    },
  ),
  at(
    ID.transition,
    settled(transition, {
      state: 'uncertain',
      answer_option_id: 'allow_once',
      finished_ms: -40000,
      reason: 'Jira did not answer within 30s; the transition may have applied. Read WRK-1874 before asking again.',
    }),
    {
      id: 'approvals-uncertain',
      since: '#401',
      title: 'Allowed, outcome uncertain',
      note: 'Red "the outcome could not be confirmed" banner.',
    },
  ),
  at(ID.egress, settled(egress, { state: 'rejected', answer_option_id: 'reject_once', reason: 'Vendor the binary instead; the session should not reach npm.' }), {
    id: 'approvals-rejected',
    title: 'Rejected with a reason',
    note: 'Dim banner, the reason, no form.',
  }),
  at(
    ID.propose,
    settled(propose, {
      state: 'changes_requested',
      answer_option_id: 'request_changes',
      operator_note: 'Drop static.rust-lang.org from egress; a toolchain bump should be its own request. Keep the rest.',
    }),
    {
      id: 'approvals-changes-requested',
      since: '#391',
      title: 'Changes requested on a setup proposal',
      note: 'Dim banner with "Notes:"; why is still rendered as a document above.',
    },
  ),
  at(ID.create, settled(create, { state: 'expired', reason: 'unanswered before it expired', decided_ms: null, finished_ms: -120000, created_ms: -1920000, expires_ms: -120000 }), {
    id: 'approvals-expired',
    title: 'Expired unanswered (settled on the node)',
    note: 'state expired, as the sweeper leaves it: dim banner, rendered Jira description, collapsed original arguments.',
  }),

  // ---- loading and missing -------------------------------------------
  {
    id: 'approvals-loading',
    area: 'approvals',
    route: '/approvals/0199f2ff-0000-7000-8000-000000000001',
    title: 'Approval loading',
    api: { 'GET /api/approvals/*': () => new Promise(() => {}) },
    sizes: ['desktop'],
  },
  {
    id: 'approvals-not-found',
    area: 'approvals',
    route: '/approvals/0199f2ff-0000-7000-8000-000000000002',
    title: 'Approval not found (404)',
    note: 'Red "not found · no such approval" banner.',
    api: {
      'GET /api/approvals/*': { status: 404, body: { error: { code: 404, message: 'no such approval' } } },
      'POST /api/approvals/*/release': { status: 404, body: { error: { code: 404, message: 'no such approval' } } },
    },
  },
  {
    id: 'approvals-server-error',
    area: 'approvals',
    route: '/approvals/0199f2ff-0000-7000-8000-000000000003',
    title: 'Approval read fails (500)',
    note: 'A server error renders the same "not found" banner as a 404.',
    api: {
      'GET /api/approvals/*': { status: 500, body: { error: { code: 500, message: 'database is locked' } } },
      'POST /api/approvals/*/release': { status: 204, body: '' },
    },
    sizes: ['desktop'],
  },

  // ---- memory batches (/promotions/:id) -------------------------------
  ...promotionStates(),
]

function promotionStates() {
  const PID = 'pb-0199f1a0-7c2e'
  const items = [
    {
      memory_id: 'm-0199f0c1',
      kind: 'lesson',
      scope: 'project',
      scope_ref: 'c3a91f0e7b24d6a8',
      body: 'In acme/orbit, `just check` runs clippy with -D warnings; run it before submit_review or the review container fails on the first lint.',
      confidence: 0.86,
      source_session: '0199f0b2-4d6e-7a8c-9e0f-1a2b3c4d5e6f',
      source_node: NODE,
      created_ms: -46800000,
    },
    {
      memory_id: 'm-0199f0c2',
      kind: 'fact',
      scope: 'project',
      scope_ref: 'c3a91f0e7b24d6a8',
      body: 'The gateway integration tests need the fixture registry; inside the boundary they time out at 120s unless its host is in the repository egress.',
      confidence: 0.72,
      source_session: '0199f0b2-4d6e-7a8c-9e0f-1a2b3c4d5e6f',
      source_node: NODE,
      created_ms: -45000000,
    },
    {
      memory_id: 'm-0199f0c3',
      kind: 'episode',
      scope: 'session',
      scope_ref: '0199f0b2-4d6e-7a8c-9e0f-1a2b3c4d5e6f',
      body: 'Rebased feat/rate-limits onto main after #398 retired the scenario links; two criterion tests had to be rewritten.',
      confidence: 0.55,
      source_session: '0199f0b2-4d6e-7a8c-9e0f-1a2b3c4d5e6f',
      source_node: NODE,
      created_ms: -41000000,
    },
    {
      memory_id: 'm-0199f0c4',
      kind: 'lesson',
      scope: 'global',
      scope_ref: null,
      body: 'Ask for egress before trying a host: a refused CONNECT already asks the operator, and retrying it queues nothing new.',
      confidence: 0.91,
      source_session: null,
      source_node: NODE,
      created_ms: -30000000,
    },
  ]
  const row = (over = {}) => ({
    id: PID,
    channel: 'personal',
    items_json: JSON.stringify(items),
    state: 'open',
    verdicts_json: null,
    decided_by: null,
    decided_ms: null,
    site: NODE,
    hlc_ms: -21600000,
    created_ms: -21600000,
    ...over,
  })
  const body = (over = {}, list = items, verdicts = {}) => ({
    promotion: row({ items_json: JSON.stringify(list), ...over }),
    items: list,
    verdicts,
  })
  const st = (id, b, rest = {}) => {
    const { api, ...more } = rest
    return { id, area: 'approvals', route: `/promotions/${PID}`, api: { [`GET /api/promotions/${PID}`]: b, ...(api ?? {}) }, ...more }
  }
  const promote = (page, i) => page.locator('.item').nth(i).getByRole('button', { name: 'Promote' }).click()
  const reject = (page, i) => page.locator('.item').nth(i).getByRole('button', { name: 'Reject' }).click()

  const partial = {
    'm-0199f0c1': { verdict: 'promote', body: 'In acme/orbit, run `just check` (clippy -D warnings) before submit_review.' },
    'm-0199f0c3': 'reject',
  }
  const many = Array.from({ length: 14 }, (_, i) => ({
    ...items[i % items.length],
    memory_id: `m-0199f0d${i.toString(16)}`,
    created_ms: -40000000 + i * 600000,
  }))
  many[2] = {
    ...many[2],
    kind: 'fact',
    body: `The release workflow publishes to ghcr.io/acme/orbit-gateway and the digest it prints is the one homelab pins: sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08ffffffffffffffffffffffffffffffffffffff and nothing else should be trusted as the deployed image.`,
  }
  many[5] = {
    ...many[5],
    kind: 'lesson',
    body: `When a review is sent back with a returned patch, apply it before addressing the notes:

1. the patch is what the operator already decided;
2. the notes are what is still open;
3. resubmitting with the same review_id keeps the history together.`,
  }

  return [
    st('approvals-promotion-open', body(), {
      title: 'Memory batch, four items waiting',
      note: 'Mixed kinds and scopes; Promote all / Reject all links; Send disabled until a verdict is picked.',
    }),
    st('approvals-promotion-picked', body(), {
      title: 'Verdicts picked, one body being edited',
      note: 'Promote turns the body into an editable textarea and the bar green; Reject dims the row; Send reads "Send 2 verdicts".',
      act: async (page) => {
        await promote(page, 0)
        await reject(page, 2)
        await page.locator('.item').nth(0).locator('textarea').fill('In acme/orbit, run `just check` (clippy -D warnings) before submit_review.')
      },
    }),
    st('approvals-promotion-promote-all', body(), {
      title: 'Promote all',
      note: 'Every pending row becomes an editable textarea at once.',
      act: async (page) => {
        await page.getByRole('button', { name: 'Promote all' }).click()
      },
      sizes: ['desktop'],
    }),
    st('approvals-promotion-partial', body({}, items, partial), {
      title: 'Partly decided earlier',
      note: 'One promoted with an edited body (object verdict), one rejected (legacy string verdict) shown as chips; two still pending.',
    }),
    st(
      'approvals-promotion-decided',
      body(
        { state: 'decided', decided_by: NODE, decided_ms: -3600000, verdicts_json: JSON.stringify({ ...partial, 'm-0199f0c2': 'promote', 'm-0199f0c4': 'promote' }) },
        items,
        { ...partial, 'm-0199f0c2': 'promote', 'm-0199f0c4': { verdict: 'promote' } },
      ),
      {
        title: 'Batch decided',
        note: '"· decided" in the heading, chips on every row, no Send row and no Back link.',
      },
    ),
    st('approvals-promotion-single', body({}, [items[3]]), {
      title: 'One item: no bulk links',
      sizes: ['desktop', 'phone'],
    }),
    st('approvals-promotion-long', body({}, many), {
      title: 'Fourteen items, long and multi-line bodies',
      note: 'An unbroken digest and a numbered multi-line lesson (pre-wrap). Check the Reject/Promote column alignment and phone wrapping.',
    }),
    st('approvals-promotion-send-error', body(), {
      title: 'Send refused (409)',
      note: 'The node\'s "no open batch by that id" in red beside Send.',
      api: { [`POST /api/promotions/${PID}/verdict`]: { status: 409, body: { error: { code: 409, message: 'no open batch by that id' } } } },
      act: async (page) => {
        await promote(page, 1)
        await page.getByRole('button', { name: /^Send/ }).click()
      },
    }),
    {
      id: 'approvals-promotion-not-found',
      area: 'approvals',
      route: '/promotions/pb-missing',
      title: 'Batch not found (404)',
      note: 'Plain "no such batch" in the empty style.',
      api: { 'GET /api/promotions/*': { status: 404, body: { error: { code: 404, message: 'no such batch' } } } },
    },
    {
      id: 'approvals-promotion-loading',
      area: 'approvals',
      route: '/promotions/pb-slow',
      title: 'Batch loading',
      api: { 'GET /api/promotions/*': () => new Promise(() => {}) },
      sizes: ['desktop'],
    },
  ]
}
