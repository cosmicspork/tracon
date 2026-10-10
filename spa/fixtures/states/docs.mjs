// Documents: the list (/docs), one document (/docs/:channel/:slug), its
// editor (/edit), the full-bleed HTML preview (/preview), and the two transfer
// panels that carry documents between nodes (TransferExport on a session,
// TransferInbox in Settings). Shapes follow node/src/http/api.rs: the list
// carries no bodies, search returns { hits, text_only }, a lost edit is a 412
// with { error, hash, body }, and a preview is a POST minting { url, expires_ms }.

import { flaky, streamEvent } from '../flaky.mjs'

const area = 'docs'

// A stable 64-hex stand-in for a content hash: FNV-1a over the whole seed,
// once per 8-hex chunk.
const hex = (seed) => {
  let out = ''
  for (let chunk = 0; out.length < 64; chunk++) {
    let h = 2166136261 ^ chunk
    for (let i = 0; i < seed.length; i++) h = Math.imul(h ^ seed.charCodeAt(i), 16777619) >>> 0
    out += h.toString(16).padStart(8, '0')
  }
  return out.slice(0, 64)
}

const KINDS = ['note', 'repo', 'meeting', 'inbox', 'proposal', 'plan', 'brief', 'context', 'guide', 'ref', 'architecture', 'shown']
const kindOf = (slug) => KINDS.find((k) => slug === k || slug.startsWith(`${k}-`)) ?? 'other'

function doc(slug, title, o = {}) {
  const channel = o.channel ?? 'personal'
  return {
    id: `doc-${hex(channel + slug).slice(0, 12)}`,
    channel,
    slug,
    kind: kindOf(slug),
    title,
    body: o.body ?? '',
    hash: o.hash ?? hex(slug + (o.body ?? '')),
    format: o.format ?? 'markdown',
    entry_path: o.entry_path ?? null,
    source_name: o.source_name ?? null,
    ...(o.bundle_files ? { bundle_files: o.bundle_files } : {}),
    site: o.site ?? '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6',
    hlc_ms: o.updated_ms ?? -3600000,
    deleted: 0,
    archived: o.archived ? 1 : 0,
    pinned: o.pinned ? 1 : 0,
    created_ms: (o.updated_ms ?? -3600000) - 86400000 * 3,
    updated_ms: o.updated_ms ?? -3600000,
  }
}

const html = (slug, title, o = {}) =>
  doc(slug, title, {
    format: 'html',
    entry_path: 'index.html',
    source_name: o.source_name ?? slug.replace(/^[a-z]+-/, ''),
    ...o,
  })

// What a channel's list holds: every kind the node knows, a pinned one, an
// HTML one, and archived ones that only show behind the toggle.
const personal = [
  doc('architecture-boundary', 'Boundary: gateway, egress and the session container', { updated_ms: -86400000 * 9 }),
  doc('architecture-mesh', 'Mesh: hub relay, sealed envelopes and HLC replication', { updated_ms: -86400000 * 21 }),
  doc('brief-rate-limits', 'Brief: per-route rate limits for the orbit API', { updated_ms: -7200000 }),
  doc('context-wi-2', 'Selected context for wi-2', { updated_ms: -5400000 }),
  doc('guide-release', 'Cutting a release with release-please', { pinned: true, updated_ms: -86400000 * 4 }),
  doc('guide-scratch-node', 'Running a scratch node beside the real one', { updated_ms: -86400000 * 2 }),
  doc('inbox-2026-10-07', 'Inbox 2026-10-07', { updated_ms: -900000 }),
  doc('meeting-2026-10-06-sync', 'Weekly sync 2026-10-06', { updated_ms: -86400000 * 2 }),
  doc('note-tracon-dogfood', 'Tracon dogfood failures', { updated_ms: -1800000 }),
  doc('note-ui-audit', 'UI audit before 0.29.0', { updated_ms: -240000 }),
  doc('plan-transfer-v2', 'Plan: candidate transfer v2 with mesh delivery', { updated_ms: -86400000 * 6 }),
  doc('proposal-docs-preview-origin', 'Proposal: a separate origin for HTML previews', { updated_ms: -86400000 * 12 }),
  doc('ref-mcp-tools', 'MCP tools exposed on an external channel', { updated_ms: -86400000 * 3 }),
  html('ref-html-preview', 'Interactive HTML preview', { source_name: 'preview-demo', updated_ms: -3600000 }),
  doc('repo-tracon', 'tracon', { updated_ms: -86400000 }),
  html('shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04', 'shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04', {
    entry_path: 'tracon-shown.html',
    source_name: 'shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04',
    updated_ms: -2700000,
  }),
  doc('scratch-ideas', 'Scratch: ideas without a kind yet', { updated_ms: -86400000 * 30 }),
]
const personalArchived = [
  doc('plan-opencode-v1-spike', 'Plan: OpenCode v1 spike (superseded)', { archived: true, updated_ms: -86400000 * 40 }),
  doc('note-qa-sidecar', 'QA sidecar notes (removed in 0.27)', { archived: true, updated_ms: -86400000 * 55 }),
]
const workDocs = [
  doc('guide-onboarding', 'Onboarding a new repository', { channel: 'work', updated_ms: -86400000 * 5 }),
  doc('ref-ci-matrix', 'CI matrix and required checks', { channel: 'work', updated_ms: -86400000 }),
]

/** GET /api/docs answered like the node: list by channel/archived, or search hits. */
const listing =
  (sets, search = () => ({ hits: [], text_only: false })) =>
  (req) => {
    if (req.query.q) return search(req.query.q)
    const ch = req.query.channel ?? 'personal'
    const all = sets[ch] ?? []
    return { docs: all.filter((d) => req.query.archived === 'true' || !d.archived).map((d) => ({ ...d, body: '' })) }
  }

const populated = listing({ personal: [...personal, ...personalArchived], work: workDocs })

// A busy channel: over a hundred notes and long, unbroken titles and slugs.
const many = (() => {
  const out = [...personal]
  for (let i = 1; i <= 48; i++) {
    out.push(doc(`note-session-${String(i).padStart(3, '0')}-handoff`, `Session handoff ${i}: what the agent left unfinished and why`, { updated_ms: -3600000 * i }))
  }
  for (let i = 1; i <= 14; i++) out.push(doc(`meeting-2026-09-${String(i + 10).padStart(2, '0')}`, `Meeting 2026-09-${i + 10}`, { updated_ms: -86400000 * (i + 3) }))
  out.push(
    doc(
      'ref-an-extremely-long-document-slug-that-someone-wrote-without-thinking-about-where-it-would-be-shown-in-the-list',
      'A reference document whose title keeps going well past any reasonable width, because it was generated from the first heading of an agent-written page that never learned brevity',
      { updated_ms: -60000 },
    ),
    doc('note-unbroken', 'sha256:2d82d875d3314d1126c0352131662e162fbc4aa33da49c30fc5ed182b933688e2d82d875d3314d1126c0352131662e', { updated_ms: -120000 }),
  )
  return out
})()

const hits = [
  {
    kind: 'document',
    id: 'doc-a1',
    format: 'markdown',
    slug: 'guide-release',
    title: 'Cutting a release with release-please',
    text: '…merging the release PR cuts the tag and the same workflow run publishes the images to GHCR, the crates and…',
    scope: null,
    confidence: null,
    rank: 0.12,
  },
  {
    kind: 'document',
    id: 'doc-a2',
    format: 'markdown',
    slug: 'plan-transfer-v2',
    title: 'Plan: candidate transfer v2 with mesh delivery',
    text: '…a release of the transfer format bumps the signing domain; old packages are refused rather than imported…',
    scope: null,
    confidence: null,
    rank: 0.31,
  },
  {
    kind: 'document',
    id: 'doc-html-preview',
    format: 'html',
    slug: 'ref-html-preview',
    title: 'Interactive HTML preview',
    text: '…Local CSS and inline JavaScript are ready. Run interaction…',
    scope: null,
    confidence: null,
    rank: 0.44,
  },
  {
    kind: 'document',
    id: 'doc-a4',
    format: 'markdown',
    slug: 'note-tracon-dogfood',
    title: 'Tracon dogfood failures',
    text: '…pr_merge returned "release branch not found" after approval; fell back to gh pr merge with the gate kept in chat, then the release-please run…',
    scope: null,
    confidence: null,
    rank: 0.52,
  },
  {
    kind: 'document',
    id: 'doc-a5',
    format: 'markdown',
    slug: 'ref-an-extremely-long-document-slug-that-someone-wrote-without-thinking-about-where-it-would-be-shown',
    title: 'A reference document whose title keeps going well past any reasonable width, because it was generated from a heading',
    text: '…release…https://github.com/example-org/example-repo/actions/runs/123456789012/job/987654321098?pr=1234&check_suite_focus=true…',
    scope: null,
    confidence: null,
    rank: 0.7,
  },
]

const typeSearch = (q) => async (page) => {
  await page.getByPlaceholder('Search by content').fill(q)
  await page.waitForTimeout(500)
}

// A document that uses everything the renderer does.
const richBody = `# Cutting a release with release-please

Releases are cut by **merging the release PR**, never by tagging by hand. The same
workflow run builds and publishes whatever the repository ships: images to GHCR,
crates, and the desktop bundles. See [the release workflow](https://github.com/example-org/tracon/blob/main/.github/workflows/release.yml)
and \`release-please-config.json\`.

> Merging a release PR is a release. Approval must name the PR and say that it cuts
> the release; approval of the feature PRs before it does not carry over.

## Before you merge

1. CI is green on the **current** head of the release PR.
2. The changelog reads as a list of things an operator would notice.
   - \`feat\` bumps the minor before 1.0.
   - \`fix\` bumps the patch.
   - Everything else accumulates without forcing a release.
3. Nobody is mid-review on a \`feat\` PR that should ride along.

- [x] \`title\`, \`spa\`, \`node\`, \`wrapper\`, \`musl\` and \`macos\` are required
- [ ] the release notes mention the policy bundle version
- [ ] the desktop app was opened on macOS once

### Which checks are required

| Check | Runs on | Typical | Timeout | What it guards |
|---|---|---|---|---|
| \`title\` | ubuntu-24.04 | 5 s | 2 min | conventional-commit PR title |
| \`spa\` | ubuntu-24.04 | 1 min 40 s | 6 min | svelte-check, vitest, build |
| \`node\` | ubuntu-24.04 | 9 min | 30 min | clippy, nextest, sqlite-vec |
| \`musl\` | ubuntu-24.04 | 12 min | 40 min | the static release binary |
| \`macos\` | macos-15 | 14 min | 45 min | desktop bundle and signing |

## After the merge

\`\`\`bash
gh run list -b main --workflow release.yml --limit 3
gh run watch "$(gh run list -b main --workflow release.yml --json databaseId -q '.[0].databaseId')"
\`\`\`

Then check the node picked the release up:

\`\`\`
$ tracon --version
tracon 0.29.0 (704ee688 2026-10-07)
\`\`\`

---

Raw HTML is shown as text, never run: <script>alert('nope')</script> and <b>bold</b>.
A link with a dangerous scheme keeps only its text: [click me](javascript:alert(1)).
`

const longBody = `# Long lines, wide tables and unbroken strings

This document checks overflow. Every block below is wider than the reading column.

A paragraph containing a long unbroken token: sha256:2d82d875d3314d1126c0352131662e162fbc4aa33da49c30fc5ed182b933688e2d82d875d3314d1126c0352131662e162fbc4aa33da49c30fc5ed182b933688e and then more words after it.

A bare URL: https://github.com/example-org/example-repo/actions/runs/123456789012/job/987654321098?pr=1234&check_suite_focus=true#step:7:1203

Inline code that does not wrap: \`/var/home/operator/src/.worktrees/tracon-feat-a-very-long-branch-name/spa/src/components/settings/CredentialSettings.svelte\`

\`\`\`rust
pub async fn put_doc(State(s): State<AppState>, Path((channel, slug)): Path<(String, String)>, headers: axum::http::HeaderMap, Json(body): Json<PutDoc>) -> axum::response::Response {
    let if_match = headers.get("if-match").and_then(|v| v.to_str().ok()).map(|v| v.trim_matches('"').to_string()); // a deliberately long line
}
\`\`\`

| Column one | Column two | Column three | Column four | Column five | Column six | Column seven | Column eight |
|---|---|---|---|---|---|---|---|
| ${'value '.repeat(6)} | ${'value '.repeat(6)} | /var/home/operator/src/tracon/node/src/http/api.rs | 0x2d82d875d3314d1126c0352131662e16 | ${'value '.repeat(4)} | ok | ok | ok |
| short | short | short | short | short | short | short | short |

![The 512 pixel app icon, wider than a phone](/icon-512.png)

${Array.from({ length: 6 }, (_, i) => `## Section ${i + 1}: a heading long enough to wrap onto a second line on a phone screen\n\nFiller paragraph ${i + 1}. The renderer keeps a 72ch reading column; this text exists so the page is long enough to scroll and the heading rhythm can be judged.`).join('\n\n')}
`

const rich = doc('guide-release', 'Cutting a release with release-please', { body: richBody, pinned: true, updated_ms: -86400000 * 4 })
const longDoc = doc(
  'ref-overflow-with-a-rather-long-slug-to-see-how-the-breadcrumb-copes',
  'Long lines, wide tables and unbroken strings',
  { body: longBody, updated_ms: -600000 },
)

const htmlDoc = html('ref-html-preview', 'Interactive HTML preview', {
  source_name: 'preview-demo',
  body: '<!doctype html><title>Interactive HTML preview</title>',
  hash: '2d82d875d3314d1126c0352131662e162fbc4aa33da49c30fc5ed182b933688e',
  bundle_files: [
    { path: 'index.html', media_type: 'text/html; charset=utf-8', size_bytes: 743 },
    { path: 'assets/app.css', media_type: 'text/css; charset=utf-8', size_bytes: 4182 },
    { path: 'assets/app.js', media_type: 'text/javascript; charset=utf-8', size_bytes: 21934 },
    { path: 'assets/chart-data.json', media_type: 'application/json', size_bytes: 118204 },
    { path: 'img/screenshot-desktop-dark.png', media_type: 'image/png', size_bytes: 1402771 },
  ],
})

const htmlLong = html(
  'ref-quarterly-usage-report-generated-by-the-metrics-agent-with-a-long-slug',
  'Quarterly usage report',
  {
    source_name: 'quarterly-usage-report-2026-q3-final-final-v2-generated-by-the-metrics-agent',
    entry_path: 'reports/2026/q3/generated/by-the-metrics-agent/without/a/short/path/index.html',
    body: '<!doctype html><title>Quarterly usage report</title>',
    bundle_files: Array.from({ length: 24 }, (_, i) => ({
      path: i === 0
        ? 'reports/2026/q3/generated/by-the-metrics-agent/without/a/short/path/index.html'
        : `reports/2026/q3/generated/by-the-metrics-agent/without/a/short/path/assets/chunk-${String(i).padStart(2, '0')}-${hex('c' + i).slice(0, 20)}.js`,
      media_type: i === 0 ? 'text/html; charset=utf-8' : 'text/javascript; charset=utf-8',
      size_bytes: 1200 * i * i + 377,
    })),
  },
)

const shownDoc = html('shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04', 'shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04', {
  source_name: 'shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04',
  entry_path: 'tracon-shown.html',
  body: '<!doctype html><title>Rate limit dashboard</title>',
  bundle_files: [
    { path: 'tracon-shown.html', media_type: 'text/html; charset=utf-8', size_bytes: 2210 },
    { path: 'target/screens/limits-desktop.png', media_type: 'image/png', size_bytes: 214882 },
    { path: 'target/screens/limits-phone.png', media_type: 'image/png', size_bytes: 98120 },
    { path: 'coverage/summary.txt', media_type: 'text/plain; charset=utf-8', size_bytes: 1904 },
  ],
})

const contextDoc = doc('context-wi-2', 'Selected context for wi-2', {
  updated_ms: -5400000,
  body: `# Selected context for wi-2

Every attempt at **wi-2 · per-route rate limits** starts with exactly this. Only the
operator changes it; a session that tries gets \`context-wi-2 is a work item's
selected context, which only the operator changes\`.

- /docs/personal/brief-rate-limits
- /docs/personal/architecture-boundary
- memory \`m-01J9Z4\`: *the API gateway already counts requests per token; reuse it*
`,
})

const html412 = (d) => ({
  status: 412,
  body: { error: { message: 'the document changed since it was read' }, hash: hex('theirs'), body: d },
})

const pending = () => new Promise(() => {})

const fileInput = (page, i = 0) => page.locator('.html-import input[type=file]').nth(i)
const pickHtml = async (page, name = 'Q3 usage report.html') => {
  await fileInput(page).setInputFiles({
    name,
    mimeType: 'text/html',
    buffer: Buffer.from('<!doctype html><title>Q3 usage</title><h1>Q3 usage</h1>'.repeat(40)),
  })
  await page.waitForTimeout(200)
}

const noDraft = { text: '', updated_ms: null }

const openTransferExport = async (page) => {
  const details = page.locator('details.transfer')
  await details.locator('summary').click()
  await details.scrollIntoViewIfNeeded()
}
const openTransferInbox = async (page) => {
  const details = page.locator('details.inbox')
  await details.locator('summary').click()
  await page.waitForTimeout(200)
  // Below the sticky section nav, so the summary stays in view.
  await details.evaluate((el) => window.scrollTo(0, el.getBoundingClientRect().top + window.scrollY - 140))
}

const transfers = [
  {
    id: 'tr-01J9ZB3K',
    candidate_id: 'cand-01J9ZA7Q4M2X-rate-limits',
    channel: 'personal',
    origin_node: '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1',
    target_node: null,
    created_ms: -1800000,
    files: 214,
    documents: 2,
    memories: 3,
    note: 'Limits are wired for /v1/*; /admin/* still needs a bucket. Tests for the 429 body are missing.',
    import_state: null,
    session_id: null,
  },
  {
    id: 'tr-01J9Y0PD',
    candidate_id: 'cand-01J9XZ55Q0DD-docs-search',
    channel: 'personal',
    origin_node: '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1',
    target_node: '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6',
    created_ms: -86400000,
    files: 88,
    documents: 1,
    memories: 0,
    note: '',
    import_state: 'imported',
    session_id: 's-0192f9c2-4a1b-7d3e-8c5f-1e2d3c4b5a69',
  },
  {
    id: 'tr-01J9X1RV',
    candidate_id: 'cand-01J9X0CC71AB-mesh-backoff',
    channel: 'work',
    origin_node: 'c3d9e1f27a8b4c5d6e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d',
    target_node: null,
    created_ms: -86400000 * 2,
    files: 41,
    documents: 0,
    memories: 1,
    note: '',
    import_state: 'preparing',
    session_id: null,
  },
  {
    id: 'tr-01J9W8KE',
    candidate_id: 'cand-01J9W7ZZ09QT-a-candidate-id-that-is-much-longer-than-the-others-because-an-agent-named-it',
    channel: 'personal',
    origin_node: '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1',
    target_node: null,
    created_ms: -86400000 * 3,
    files: 12,
    documents: 4,
    memories: 7,
    note: 'Retry once the work-pod has the toolchain image; the first import ran out of disk while preparing the workspace and nothing was started.',
    import_state: 'failed',
    session_id: null,
  },
]

// What the Settings page reads before its maintenance section settles; only
// here so the transfer inbox renders on a page without missing fixtures.
const settingsSupport = {
  '/api/config': {
    node_name: 'laptop',
    harness: { id: 'claude', version: '2.5.0', tools: [] },
    session: { budget_tokens: 2000000, permission_timeout_secs: 600, default_channel: 'personal' },
    review: { max_diff_lines: 4000, max_files: 120 },
    gateway: { allow_hosts: [] },
    publish: { gh: '', glab: '', git: '' },
    boundary: { podman: '' },
    external: { enabled: true },
    launch: { plugins: [] },
    providers: {},
    readonly: { hub_url: 'https://hub.example.net', runtime: 'podman', config_path: '/home/op/.config/tracon/node.toml' },
    running: { harness_id: 'claude', harness_version: '2.5.0', node_name: 'laptop' },
  },
  '/api/authority/grants': { policy: { version: 12, rules: [], trusted: true }, grants: [] },
  '/api/admin/access': { authenticated: false, token_configured: true, local: true },
}

export default [
  // ---- the list ----
  {
    id: 'docs-list',
    area,
    route: '/docs',
    title: 'Documents grouped by kind',
    note: 'Every kind the node writes is present, context-wi-2 under context, and the header count matches the rows. Pinned doc carries no marker in the list.',
    api: { '/api/docs': populated },
  },
  {
    id: 'docs-list-many',
    area,
    route: '/docs',
    title: 'A busy channel with long titles and slugs',
    note: 'Ellipsis on long titles and an unbroken sha title; group headers and counts with 80 docs.',
    api: { '/api/docs': listing({ personal: many }) },
  },
  {
    id: 'docs-list-empty',
    area,
    route: '/docs',
    title: 'No documents on the channel',
    api: { '/api/docs': listing({}) },
  },
  {
    id: 'docs-list-loading',
    area,
    route: '/docs',
    title: 'List still loading',
    note: 'Says it is loading, with no count in the header and no empty-state copy.',
    api: { '/api/docs': pending },
  },
  {
    id: 'docs-list-error',
    area,
    route: '/docs',
    title: 'List failed to load',
    note: 'The error with a Retry; no count in the header and no empty-state copy beside it.',
    api: { '/api/docs': { status: 500, body: { error: { code: 500, message: 'database is locked' } } } },
  },
  {
    id: 'docs-list-refresh-error',
    area,
    route: '/docs',
    title: 'List loaded, then a refresh failed',
    note: 'A document change on the stream refetches and that request fails: the list stays, under "Could not refresh documents" with a Retry.',
    api: { '/api/docs': populated },
    init: flaky('/api/docs', [2]),
    act: async (page) => {
      await page.locator('.h5').first().waitFor()
      await streamEvent(page, 'changes', { type: 'changes', changes: [{ table: 'document' }] })
      await page.getByText('Could not refresh documents').waitFor()
    },
  },
  {
    id: 'docs-list-archived',
    area,
    route: '/docs',
    title: 'Show archived toggled on',
    note: 'Archived documents gather in their own group at the end.',
    api: { '/api/docs': populated },
    act: async (page) => {
      await page.getByLabel('Show archived').check()
      await page.waitForTimeout(300)
    },
  },
  {
    id: 'docs-list-other-channel',
    area,
    route: '/docs',
    title: 'Switched to the work channel',
    api: { '/api/docs': populated },
    act: async (page) => {
      await page.locator('.bar select').selectOption('work')
      await page.waitForTimeout(300)
    },
  },
  {
    id: 'docs-list-hub-down',
    area,
    route: '/docs',
    title: 'Hub unreachable, search is local',
    api: {
      '/api/docs': populated,
      '/api/mesh': { hub: { state: 'unreachable' }, hub_url: 'https://hub.example.net', node_id: '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6' },
    },
  },
  {
    id: 'docs-search',
    area,
    route: '/docs',
    title: 'Search results',
    note: 'Kind label taken from the slug prefix, HTML badge, snippets with ellipses; a long title and snippet.',
    api: { '/api/docs': listing({ personal }, () => ({ hits, text_only: false })) },
    act: typeSearch('release'),
  },
  {
    id: 'docs-search-text-only',
    area,
    route: '/docs',
    title: 'Search answered without the embedding endpoint',
    note: 'Header says "text only · no semantic search".',
    api: { '/api/docs': listing({ personal }, () => ({ hits: hits.slice(0, 2), text_only: true })) },
    act: typeSearch('release'),
  },
  {
    id: 'docs-search-empty',
    area,
    route: '/docs',
    title: 'Search with no matches',
    api: { '/api/docs': listing({ personal }, () => ({ hits: [], text_only: false })) },
    act: typeSearch('kubernetes ingress'),
  },
  {
    id: 'docs-new',
    area,
    route: '/docs',
    title: 'New Markdown form with a slug typed',
    note: 'The kinds hint omits brief, context and shown.',
    api: { '/api/docs': populated },
    act: async (page) => {
      await page.getByRole('button', { name: 'New Markdown' }).click()
      await page.getByPlaceholder('kind-slug, e.g. guide-deploy').fill('guide-ui-audit')
    },
  },
  {
    id: 'docs-new-from-empty',
    area,
    route: '/docs',
    title: 'Empty channel, Write Markdown pressed',
    note: 'Submit stays disabled until a slug is typed.',
    api: { '/api/docs': listing({}) },
    act: async (page) => {
      await page.locator('.empty').getByRole('button', { name: 'Write Markdown' }).click()
    },
  },
  {
    id: 'docs-import',
    area,
    route: '/docs',
    title: 'Import HTML panel, nothing chosen',
    api: { '/api/docs': populated },
    act: async (page) => {
      await page.locator('.h4 .actions').getByRole('button', { name: 'Import HTML' }).click()
    },
  },
  {
    id: 'docs-import-selected',
    area,
    route: '/docs',
    title: 'Import HTML with a file chosen',
    note: 'Summary row, suggested slug (ref- prefixed), Import button.',
    api: { '/api/docs': populated },
    act: async (page) => {
      await page.locator('.h4 .actions').getByRole('button', { name: 'Import HTML' }).click()
      await pickHtml(page)
    },
  },
  {
    id: 'docs-import-invalid',
    area,
    route: '/docs',
    title: 'Import HTML with a non-HTML file',
    api: { '/api/docs': populated },
    act: async (page) => {
      await page.locator('.h4 .actions').getByRole('button', { name: 'Import HTML' }).click()
      await fileInput(page).setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('plain text') })
      await page.waitForTimeout(200)
    },
  },
  {
    id: 'docs-import-failed',
    area,
    route: '/docs',
    title: 'Import HTML refused by the node',
    api: {
      '/api/docs': populated,
      'POST /api/docs/*/*/html': { status: 400, body: { error: { code: 400, message: 'slug "ref-q3 usage" is not usable' } } },
    },
    act: async (page) => {
      await page.locator('.h4 .actions').getByRole('button', { name: 'Import HTML' }).click()
      await pickHtml(page)
      await page.locator('.html-import .slug input').fill('ref-q3 usage')
      await page.locator('.html-import').getByRole('button', { name: 'Import HTML' }).click()
      await page.waitForTimeout(300)
    },
  },

  // ---- one Markdown document ----
  {
    id: 'docs-doc',
    area,
    route: '/docs/personal/guide-release',
    title: 'Markdown document, everything the renderer does',
    note: 'Headings, lists, task list, table, code, blockquote, hr, escaped raw HTML, a neutralised javascript: link. Phone hides Edit/Pin/Archive/Delete.',
    api: { '/api/docs/personal/guide-release': rich },
  },
  {
    id: 'docs-doc-overflow',
    area,
    route: '/docs/personal/ref-overflow-with-a-rather-long-slug-to-see-how-the-breadcrumb-copes',
    title: 'Markdown with long lines, wide table, big image',
    note: 'Unbroken token and URL in a paragraph, wide code and table, a 512px image, and a long slug in the breadcrumb.',
    api: { '/api/docs/personal/ref-overflow-with-a-rather-long-slug-to-see-how-the-breadcrumb-copes': longDoc },
  },
  {
    id: 'docs-doc-archived-pinned',
    area,
    route: '/docs/personal/guide-release',
    title: 'Archived and pinned document',
    note: 'Header reads "· archived · pinned"; actions read Unpin and Unarchive.',
    api: { '/api/docs/personal/guide-release': { ...rich, archived: 1, pinned: 1 } },
  },
  {
    id: 'docs-doc-context',
    area,
    route: '/docs/personal/context-wi-2',
    title: 'A work item\'s selected context',
    note: 'Operator-owned (sessions may not write it), but the page shows no hint of that and offers the usual Edit.',
    api: { '/api/docs/personal/context-wi-2': contextDoc },
  },
  {
    id: 'docs-doc-action-failed',
    area,
    route: '/docs/personal/guide-release',
    title: 'Pin refused',
    note: 'The error is appended to the header meta line.',
    api: {
      '/api/docs/personal/guide-release': { ...rich, pinned: 0 },
      'PUT /api/docs/personal/guide-release': { status: 500, body: { error: { code: 500, message: 'database is locked' } } },
    },
    sizes: ['desktop'],
    act: async (page) => {
      await page.locator('.h4 .r').getByRole('button', { name: 'Pin' }).click()
      await page.waitForTimeout(300)
    },
  },
  {
    id: 'docs-doc-missing',
    area,
    route: '/docs/personal/guide-not-written-yet',
    title: 'Document not found',
    api: { '/api/docs/personal/guide-not-written-yet': { status: 404, body: { error: { code: 404, message: 'no document guide-not-written-yet on personal' } } } },
  },
  {
    id: 'docs-doc-error',
    area,
    route: '/docs/personal/guide-release',
    title: 'Document failed to load',
    api: { '/api/docs/personal/guide-release': { status: 500, body: { error: { code: 500, message: 'database disk image is malformed' } } } },
  },
  {
    id: 'docs-doc-loading',
    area,
    route: '/docs/personal/guide-release',
    title: 'Document loading',
    api: { '/api/docs/personal/guide-release': pending },
    sizes: ['desktop'],
  },

  // ---- the editor ----
  {
    id: 'docs-edit',
    area,
    route: '/docs/personal/guide-release/edit',
    title: 'Editing, a change typed',
    note: 'The editor fills the page below the header, scrolling inside itself; Save and Cancel sit at the foot of the window with no resize handle.',
    api: { '/api/docs/personal/guide-release': rich },
    act: async (page) => {
      await page.locator('textarea').press('End')
      await page.locator('textarea').pressSequentially('\n\nOne more line.')
    },
  },
  {
    id: 'docs-edit-new',
    area,
    route: '/docs/personal/note-ui-audit-findings/edit',
    title: 'Writing a new document',
    note: 'Header says "· new"; the draft starts from the slug; Save is disabled until it changes.',
    api: { '/api/docs/personal/note-ui-audit-findings': { status: 404, body: { error: { code: 404, message: 'no document note-ui-audit-findings on personal' } } } },
  },
  {
    id: 'docs-edit-conflict',
    area,
    route: '/docs/personal/guide-release/edit',
    title: 'Save lost to another edit (412)',
    note: 'Banner offers take theirs / keep mine.',
    api: {
      '/api/docs/personal/guide-release': rich,
      'PUT /api/docs/personal/guide-release': html412(richBody.replace('Before you merge', 'Before anyone merges')),
    },
    act: async (page) => {
      await page.locator('textarea').press('End')
      await page.locator('textarea').pressSequentially('\nmine')
      await page.getByRole('button', { name: 'Save' }).click()
      await page.waitForTimeout(300)
    },
  },
  {
    id: 'docs-edit-save-failed',
    area,
    route: '/docs/personal/note-ui-audit-findings/edit',
    title: 'Save refused (bad slug)',
    api: {
      '/api/docs/personal/note-ui-audit-findings': { status: 404, body: { error: { code: 404, message: 'no document' } } },
      'PUT /api/docs/personal/note-ui-audit-findings': { status: 400, body: { error: { code: 400, message: 'slug "note-ui-audit-findings" is not usable' } } },
    },
    act: async (page) => {
      await page.locator('textarea').pressSequentially('First finding: ')
      await page.getByRole('button', { name: 'Save' }).click()
      await page.waitForTimeout(300)
    },
  },

  // ---- HTML documents ----
  {
    id: 'docs-html',
    area,
    route: '/docs/personal/ref-html-preview',
    title: 'HTML document with its bundle files open',
    note: 'Meta row, actions, sandboxed preview, bundle file list.',
    api: { '/api/docs/personal/ref-html-preview': htmlDoc },
    act: async (page) => {
      await page.locator('details summary').click()
    },
  },
  {
    id: 'docs-html-long',
    area,
    route: '/docs/personal/ref-quarterly-usage-report-generated-by-the-metrics-agent-with-a-long-slug',
    title: 'HTML document with long names and many files',
    note: 'Long source name, entry path and hash in the meta row; 24 long bundle paths.',
    api: {
      '/api/docs/personal/ref-quarterly-usage-report-generated-by-the-metrics-agent-with-a-long-slug': htmlLong,
      'POST /api/docs/*/*/preview': { url: '/html-preview-fixture.html', expires_ms: 3600000 },
    },
    act: async (page) => {
      await page.locator('details summary').click()
    },
  },
  {
    id: 'docs-html-shown',
    area,
    route: '/docs/personal/shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04',
    title: 'Work shown by an agent (show_work)',
    note: 'A shown-* document has no special rendering: the slug is the title, and Replace/Delete are offered though only show_work writes it.',
    api: {
      '/api/docs/personal/shown-0192f3a1-7c4e-7b9a-9d2e-5f8a1c3b6e04': shownDoc,
      'POST /api/docs/*/*/preview': { url: '/html-preview-fixture.html', expires_ms: 3600000 },
    },
  },
  {
    id: 'docs-html-replace',
    area,
    route: '/docs/personal/ref-html-preview',
    title: 'Replace bundle, file chosen',
    note: 'Slug locked to the document; button reads Replace bundle.',
    api: { '/api/docs/personal/ref-html-preview': htmlDoc },
    act: async (page) => {
      await page.locator('.html-actions').getByRole('button', { name: 'Replace bundle' }).click()
      await pickHtml(page, 'index.html')
    },
  },
  {
    id: 'docs-html-replace-conflict',
    area,
    route: '/docs/personal/ref-html-preview',
    title: 'Replace bundle lost to another change (412)',
    api: {
      '/api/docs/personal/ref-html-preview': htmlDoc,
      'POST /api/docs/personal/ref-html-preview/html': html412(''),
    },
    sizes: ['desktop'],
    act: async (page) => {
      await page.locator('.html-actions').getByRole('button', { name: 'Replace bundle' }).click()
      await pickHtml(page, 'index.html')
      await page.locator('.html-import').getByRole('button', { name: 'Replace bundle' }).click()
      await page.waitForTimeout(300)
    },
  },
  {
    id: 'docs-html-preview-refused',
    area,
    route: '/docs/personal/ref-html-preview',
    title: 'HTML document whose preview the node refuses',
    note: '409 from POST /preview when the UI is served over HTTPS without a preview origin.',
    api: {
      '/api/docs/personal/ref-html-preview': htmlDoc,
      'POST /api/docs/personal/ref-html-preview/preview': {
        status: 409,
        body: { error: { code: 409, message: 'docs.preview_url must name a separate HTTPS preview origin when the operator UI uses HTTPS' } },
      },
    },
  },

  // ---- full-bleed preview ----
  {
    id: 'docs-preview',
    area,
    route: '/docs/personal/ref-html-preview/preview',
    title: 'Full-window HTML preview',
    full: false,
  },
  {
    id: 'docs-preview-long-slug',
    area,
    route: '/docs/personal/ref-quarterly-usage-report-generated-by-the-metrics-agent-with-a-long-slug/preview',
    title: 'Full-window preview, long slug',
    api: { 'POST /api/docs/*/*/preview': { url: '/html-preview-fixture.html', expires_ms: 3600000 } },
    full: false,
  },
  {
    id: 'docs-preview-loading',
    area,
    route: '/docs/personal/ref-html-preview/preview',
    title: 'Full-window preview minting',
    note: '"Loading preview…" uses the failure (crit) colour.',
    api: { 'POST /api/docs/personal/ref-html-preview/preview': pending },
    full: false,
  },
  {
    id: 'docs-preview-error',
    area,
    route: '/docs/personal/ref-markdown-only/preview',
    title: 'Full-window preview of a non-HTML document',
    api: { 'POST /api/docs/personal/ref-markdown-only/preview': { status: 409, body: { error: { code: 409, message: 'document is not HTML' } } } },
    full: false,
  },

  // ---- transfers (render on a session and in Settings) ----
  {
    id: 'docs-transfer-export',
    area,
    route: '/sessions/s-run',
    title: 'Continue on another node, filled in',
    note: 'TransferExport on a session page: destination select, document slugs, handoff note. Inputs keep browser default styling.',
    api: { '/api/sessions/s-run/draft': noDraft },
    full: false,
    act: async (page) => {
      await openTransferExport(page)
      const d = page.locator('details.transfer')
      await d.getByPlaceholder('Immutable candidate ID').fill('cand-01J9ZA7Q4M2X-rate-limits')
      await d.locator('select').selectOption({ index: 1 })
      await d.getByPlaceholder('guide-release, ref-api').fill('brief-rate-limits, architecture-boundary')
      await d.getByPlaceholder('What the next session should know').fill('Limits are wired for /v1/*; /admin/* still needs a bucket.')
      await d.evaluate((el) => el.scrollIntoView({ block: 'start' }))
    },
  },
  {
    id: 'docs-transfer-export-failed',
    area,
    route: '/sessions/s-run',
    title: 'Continue on another node, refused',
    note: 'The error should read as an error; `.transfer > p` overrides `.bad`, so it is grey.',
    api: {
      '/api/sessions/s-run/draft': noDraft,
      'POST /api/transfers': {
        status: 500,
        body: { error: { code: 500, message: 'selected document guide-release-notes is not a live Markdown document on the candidate channel' } },
      },
    },
    full: false,
    act: async (page) => {
      await openTransferExport(page)
      const d = page.locator('details.transfer')
      await d.getByPlaceholder('Immutable candidate ID').fill('cand-01J9ZA7Q4M2X-rate-limits')
      await d.getByPlaceholder('guide-release, ref-api').fill('guide-release-notes')
      await d.getByRole('button', { name: 'Download signed package' }).click()
      await page.waitForTimeout(300)
      await d.evaluate((el) => el.scrollIntoView({ block: 'start' }))
    },
  },
  {
    id: 'docs-transfer-inbox',
    area,
    route: '/sessions',
    title: 'Import a session, staged packages in every state',
    note: 'Fresh, imported, outcome unknown, failed-with-retry; a long candidate id and note.',
    api: { ...settingsSupport, '/api/transfers': { transfers } },
    full: false,
    act: openTransferInbox,
  },
  {
    id: 'docs-transfer-inbox-empty',
    area,
    route: '/sessions',
    title: 'Import a session, nothing staged',
    api: { ...settingsSupport, '/api/transfers': { transfers: [] } },
    full: false,
    act: openTransferInbox,
  },
  {
    id: 'docs-transfer-inbox-error',
    area,
    route: '/sessions',
    title: 'Import a session, list failed',
    note: 'The empty-state copy shows beside the error, and the error is grey (`.inbox > p` beats `.bad`).',
    api: { ...settingsSupport, '/api/transfers': { status: 500, body: { error: { code: 500, message: 'transfer inbox is unreadable: permission denied' } } } },
    full: false,
    act: openTransferInbox,
  },
]
