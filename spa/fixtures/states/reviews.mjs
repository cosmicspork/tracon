// The review history (/reviews): decided reviews, newest first, from this
// node and the ones it mirrors, filtered to external submissions or a
// channel, a page at a time.

const SELF = '9f31c6a870d24b5e8c1f0a6d3e7b2905a4c8d1e6f0b3a7c2d5e8f1a4b7c0d3e6'
const PEER = '4b8e2d90c1f6a35720e9d4b8a1c5f3e7d0b6a2c8e4f1d7b3a9c5e0f2d8b4a6c1'
const area = 'reviews'

function row(n, o = {}) {
  return {
    id: `0190c8a2-1c4e-7a3b-9d2f-00000000000${n}`,
    node_id: SELF,
    held_here: true,
    channel: 'personal',
    session_id: null,
    lane: 'laptop',
    title: 'Clamp the gateway refill at capacity',
    provider: 'github',
    project: 'op/orbit',
    branch: 'feat/rate-limits',
    state: 'approved',
    publish_result: null,
    decided_ms: -n * 3600000,
    ...o,
  }
}

const ROWS = [
  row(1, { publish_result: 'https://github.com/op/orbit/pull/149' }),
  row(2, { state: 'rejected', title: 'Retry the import on a locked database', lane: null }),
  row(3, {
    session_id: '0190c8a2-1c4e-7a3b-9d2f-6e8b4a2c1f07',
    lane: null,
    channel: 'work',
    title: 'Document the [gateway] per_minute key',
    publish_result: 'https://gitlab.example.com/op/orbit/-/merge_requests/12',
  }),
  row(4, {
    node_id: PEER,
    held_here: false,
    channel: 'work',
    title: 'Mirrored from the build box: bump the harness image to 2.5.0 and pin its digest in the recipe',
    branch: 'chore/harness-image-2.5.0-with-a-long-branch-name',
  }),
  row(5, { state: 'rejected', title: 'Drop the legacy podman switches' }),
]

/** The page the screen asks for, as the node would answer it. */
const history = ({ query }) => {
  let rows = ROWS
  if (query.source === 'external') rows = rows.filter((r) => !r.session_id)
  if (query.channel) rows = rows.filter((r) => r.channel === query.channel)
  return { reviews: rows, next: query.before ? null : '1:more' }
}

export default [
  {
    id: 'reviews-history',
    area,
    route: '/reviews',
    title: 'Decided reviews, newest first',
    since: '#1156',
    note: 'Approved and rejected, from an external harness, a session and a peer (its chip names the node); a PR or MR link once published; Load more below.',
    api: { 'GET /api/reviews': history },
  },
  {
    id: 'reviews-history-external',
    area,
    route: '/reviews',
    title: 'Decided reviews, external only',
    api: { 'GET /api/reviews': history },
    act: async (page) => {
      await page.getByLabel('external only').check()
    },
  },
  {
    id: 'reviews-history-empty',
    area,
    route: '/reviews',
    title: 'No decided reviews yet',
    api: { 'GET /api/reviews': { reviews: [], next: null } },
  },
  {
    id: 'reviews-history-failed',
    area,
    route: '/reviews',
    title: 'The node failed listing reviews',
    api: { 'GET /api/reviews': { status: 500, body: { error: { code: 500, message: 'database is locked' } } } },
  },
]
