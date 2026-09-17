import { expect, test } from 'bun:test'
import { attention, intentLabel, issueVerdict, permissionVerdict, reviewVerdict } from './attention'
import type {
  MeshState,
  NodeInfo,
  OperatorIssue,
  OperatorQuestion,
  Permission,
  Promotion,
  Review,
} from './types'

const NOW = 1_700_000_000_000

const node = (over: Partial<NodeInfo> = {}): NodeInfo => ({
  id: 'self',
  name: 'self',
  state: 'ready',
  failed_check: null,
  failed_detail: null,
  harness: { id: 'opencode', pinned: '1', found: '1', mismatch: false },
  models: [],
  checked_at_ms: NOW,
  is_self: true,
  reachable: true,
  last_seen_ms: NOW,
  ...over,
})

const mesh = (state: MeshState['hub']): MeshState => ({
  hub: state,
  hub_url: null,
  node_id: 'self',
  fingerprint: null,
  last_ok_ms: null,
  queued: 0,
  delivered_since_reconnect: 0,
  undecryptable: 0,
  held: 0,
  last_error: null,
  last_refusal: null,
})

const permission = (over: Partial<Permission> = {}): Permission => ({
  id: 'p1',
  session_id: 's1',
  node_id: 'self',
  title: 'run the tests',
  kind: 'tool',
  raw_input: null,
  options: '[]',
  state: 'new',
  created_ms: NOW - 1000,
  expires_ms: NOW + 60_000,
  intent: {
    channel: 'main',
    phase: 'execute',
    branch: 'tracon/p1',
    work_item_id: 'w1',
    work_item_title: 'Make the attention count mean something',
    session_state: 'waiting_on_you',
  },
  ...over,
})

const review = (over: Partial<Review> = {}): Review =>
  ({
    id: 'r1',
    session_id: 's1',
    node_id: 'self',
    channel: 'main',
    kind: 'code',
    title: 'a candidate',
    state: 'new',
    created_ms: NOW - 2000,
    ...over,
  }) as Review

const issue = (over: Partial<OperatorIssue> = {}): OperatorIssue =>
  ({
    id: 'i1',
    session_id: 's1',
    channel: 'main',
    title: 'a defect',
    body: '',
    attachments_json: '[]',
    state: 'draft',
    published_url: null,
    publish_error: null,
    created_ms: NOW - 3000,
    approved_ms: null,
    ...over,
  }) as OperatorIssue

const question = (over: Partial<OperatorQuestion> = {}): OperatorQuestion =>
  ({
    id: 'q1',
    session_id: 's1',
    channel: 'main',
    node_id: 'self',
    prompt: 'which one?',
    choices_json: '[]',
    state: 'unanswered',
    answer_json: null,
    created_ms: NOW - 4000,
    answered_ms: null,
    ...over,
  }) as OperatorQuestion

const promotion = (over: Partial<Promotion> = {}): Promotion =>
  ({
    id: 'm1',
    channel: 'main',
    items_json: '[]',
    state: 'open',
    verdicts_json: null,
    decided_by: null,
    decided_ms: null,
    site: 'self',
    hlc_ms: NOW,
    created_ms: NOW - 5000,
    ...over,
  }) as Promotion

test('a live request on a reachable node is a decision', () => {
  expect(permissionVerdict(permission(), null, NOW)).toEqual({ lane: 'decision', reason: null })
})

test('expiry is a denial, not a deferral: a lapsed request stops being a decision', () => {
  const lapsed = permissionVerdict(permission({ expires_ms: NOW }), null, NOW)
  expect(lapsed.lane).toBe('agent')
  expect(lapsed.reason).toContain('denied by default')
  // One millisecond earlier it is still the operator's to answer.
  expect(permissionVerdict(permission({ expires_ms: NOW + 1 }), null, NOW).lane).toBe('decision')
})

test('a request whose session has ended can no longer be allowed', () => {
  const p = permission({ intent: { ...permission().intent!, session_state: 'closed' } })
  expect(permissionVerdict(p, null, NOW).lane).toBe('agent')
})

test('a request on a node that cannot be reached is outside, not yours', () => {
  const verdict = permissionVerdict(permission(), 'node unreachable', NOW)
  expect(verdict.lane).toBe('external')
  expect(verdict.reason).toContain('node unreachable')
})

test('reviews split by who holds them', () => {
  expect(reviewVerdict(review(), null).lane).toBe('decision')
  // Claimed is this operator reading it; it is still theirs.
  expect(reviewVerdict({ ...review(), state: 'claimed' }, null).lane).toBe('decision')
  expect(reviewVerdict({ ...review(), state: 'revising' }, null).lane).toBe('agent')
  expect(reviewVerdict({ ...review(), state: 'publishing' }, null).lane).toBe('external')
  expect(reviewVerdict(review(), 'hub unreachable').lane).toBe('external')
})

test('issue drafts: authorize is yours, publication and its uncertainty are not', () => {
  expect(issueVerdict(issue())?.lane).toBe('decision')
  expect(issueVerdict(issue({ state: 'publishing' }))?.lane).toBe('external')
  expect(issueVerdict(issue({ state: 'uncertain' }))?.reason).toContain('reconcile')
  // A published draft is done and belongs in no lane.
  expect(issueVerdict(issue({ state: 'published' }))).toBeNull()
})

test('the count is the decision lane, and nothing is dropped from the other two', () => {
  const bay = attention({
    questions: [question(), question({ id: 'q2', state: 'answered' })],
    permissions: [permission(), permission({ id: 'p2', expires_ms: NOW - 1 })],
    reviews: [review(), { ...review(), id: 'r2', state: 'revising' }, { ...review(), id: 'r3', state: 'publishing' }],
    issues: [issue(), issue({ id: 'i2', state: 'uncertain' }), issue({ id: 'i3', state: 'published' })],
    promotions: [promotion()],
    nodes: [node()],
    mesh: mesh({ state: 'connected' }),
    now: NOW,
  })
  // question, live request, new review, issue draft, promotion.
  expect(bay.count).toBe(5)
  expect(bay.decisions).toHaveLength(5)
  expect(bay.agent.map((t) => t.key)).toEqual(['permission:p2', 'review:r2'])
  expect(bay.external.map((t) => t.key)).toEqual(['review:r3', 'issue:i2'])
  // An answered question and a published draft are finished, not hidden work.
  const keys = [...bay.decisions, ...bay.agent, ...bay.external].map((t) => t.key)
  expect(keys).not.toContain('question:q2')
  expect(keys).not.toContain('issue:i3')
})

test('an unreachable hub empties the count without emptying the bay', () => {
  const peer = node({ id: 'peer', is_self: false, reachable: true })
  const bay = attention({
    permissions: [permission({ node_id: 'peer' })],
    reviews: [{ ...review(), node_id: 'peer' }],
    nodes: [node(), peer],
    mesh: mesh({ state: 'unreachable', since_ms: NOW - 60_000 }),
    now: NOW,
  })
  expect(bay.count).toBe(0)
  expect(bay.external).toHaveLength(2)
  expect(bay.external.every((t) => t.reason?.includes('hub unreachable'))).toBe(true)
})

test('requests are answered before reviews, oldest first within a kind', () => {
  const bay = attention({
    reviews: [review({ id: 'r1' })],
    permissions: [
      permission({ id: 'late', created_ms: NOW - 10 }),
      permission({ id: 'early', created_ms: NOW - 9000 }),
    ],
    nodes: [node()],
    now: NOW,
  })
  expect(bay.decisions.map((t) => t.key)).toEqual(['permission:early', 'permission:late', 'review:r1'])
})

test('a card says what the session was started to do, and copes when it cannot', () => {
  expect(intentLabel(permission())).toBe(
    'Make the attention count mean something · execute · main · tracon/p1',
  )
  expect(intentLabel(permission({ intent: { ...permission().intent!, work_item_title: null } }))).toContain(
    'no work item',
  )
  expect(intentLabel(permission({ intent: null }))).toBe('intent unavailable')
})
