import { expect, test } from 'bun:test'
import { attemptState, carries, closeOffer, verbs, workLinks } from './continuation'
import type { Continuation, ContinuationAttempt } from './types'

const attempt = (over: Partial<ContinuationAttempt>): ContinuationAttempt => ({
  id: 'aaaaaaaa-1',
  phase: 'execute',
  model: 'm/a',
  harness: 'claude',
  state: 'closed',
  end_reason: null,
  last_error: null,
  tokens_used: 0,
  created_ms: 1,
  continued_from: null,
  parent_session: null,
  archived: false,
  ...over,
})

test('an attempt says how it ended, and why when it failed', () => {
  expect(attemptState(attempt({ end_reason: 'node_restart' }))).toBe('node restarted')
  expect(attemptState(attempt({ end_reason: 'error', last_error: 'tests red' }))).toBe('failed: tests red')
  expect(attemptState(attempt({ end_reason: 'something_new' }))).toBe('something new')
  expect(attemptState(attempt({ state: 'waiting_on_you' }))).toBe('waiting on you')
})

test('lineage points only at attempts in the same work', () => {
  const a = attempt({ id: 'aaaaaaaa-1' })
  const b = attempt({ id: 'bbbbbbbb-2', continued_from: 'aaaaaaaa-1' })
  const stray = attempt({ id: 'cccccccc-3', parent_session: 'elsewhere' })
  expect(carries(a, [a, b])).toBe(null)
  expect(carries(b, [a, b])).toBe('aaaaaaaa')
  expect(carries(stray, [a, stray])).toBe(null)
})

test('the verbs follow what the node allows', () => {
  const base = {
    next: { kind: 'continue', text: '', session_id: 'b' },
    actions: { continue_from: 'b', abandon: true },
  } as unknown as Continuation
  expect(verbs(base)).toEqual({ continue: true, changeApproach: true, abandon: true })
  expect(verbs({ ...base, next: { ...base.next, kind: 'change_approach' } })).toEqual({
    continue: false,
    changeApproach: true,
    abandon: true,
  })
  expect(verbs({ ...base, actions: { continue_from: null, abandon: false, close: false } })).toEqual({
    continue: false,
    changeApproach: false,
    abandon: false,
  })
})

const item = (over: Partial<Continuation> = {}): Continuation => ({
  kind: 'item',
  id: 'wi-2',
  channel: 'personal',
  intent: { title: 'Rate-limit the export', body: '', source: 'item' },
  attempts: [attempt({ id: 'aaaaaaaa-1', phase: 'plan', end_reason: 'phase_done' }), attempt({ id: 'bbbbbbbb-2' })],
  blockers: [],
  next: { kind: 'done', text: '', session_id: null },
  workspace: { id: 'bbbbbbbb-2', branch: 'feat/rate-limits', session_id: 'bbbbbbbb-2' },
  decisions: { plan: 'plan-wi-2', answered: [] },
  evidence: {
    reviews: [
      {
        id: 'rev-7',
        session_id: 'bbbbbbbb-2',
        title: 'feat: rate limits',
        state: 'published',
        verdict_reason: null,
        publish_result: 'https://github.com/o/r/pull/149',
        forge_state: 'merged',
        head_sha: 'abc',
        created_ms: 5,
      },
    ],
    shown: [],
  },
  actions: { continue_from: null, abandon: false, close: false },
  follow: null,
  ...over,
})

const read = (groups: ReturnType<typeof workLinks>) =>
  groups.map((g) => [g.label, ...g.links.map((l) => [l.text, l.note ?? '', l.href ?? ''].filter(Boolean).join(' '))].filter(Boolean).join(': '))

test('the links bar names everything linked to the work, in order', () => {
  expect(read(workLinks(item(), { sessionId: 'bbbbbbbb-2' }))).toEqual([
    'Task /tasks/wi-2',
    'Plan doc /docs/personal/plan-wi-2',
    'Sessions: plan aaaaaaaa /sessions/aaaaaaaa-1: execute bbbbbbbb /sessions/bbbbbbbb-2',
    'Review published /reviews/rev-7',
    'PR #149 merged https://github.com/o/r/pull/149',
    'Branch: feat/rate-limits',
  ])
  const here = workLinks(item(), { sessionId: 'bbbbbbbb-2' }).find((g) => g.label === 'Sessions')!.links
  expect(here.map((l) => l.here)).toEqual([false, true])
})

test('the links bar leaves out what the work does not have, and the task on its own page', () => {
  const bare = item({
    attempts: [],
    workspace: null,
    decisions: { plan: null, answered: [] },
    evidence: { reviews: [], shown: [] },
  })
  expect(read(workLinks(bare, { itemId: 'wi-2' }))).toEqual([])
  // A review not yet published, or published without an address, has no PR.
  const pushed = item({ evidence: { reviews: [{ ...item().evidence.reviews[0], state: 'approved', publish_result: 'pushed abc' }], shown: [] } })
  expect(read(workLinks(pushed, { itemId: 'wi-2' }))).toEqual([
    'Plan doc /docs/personal/plan-wi-2',
    'Sessions: plan aaaaaaaa /sessions/aaaaaaaa-1: execute bbbbbbbb /sessions/bbbbbbbb-2',
    'Review approved /reviews/rev-7',
    'Branch: feat/rate-limits',
  ])
})

test('closing the task is offered while it is open, prominently once its request is merged', () => {
  const base = {
    next: { kind: 'merge', text: 'PR #149 is open · merge it', session_id: 's' },
    actions: { continue_from: 's', abandon: true, close: true },
    follow: { url: 'u', label: 'PR #149', state: 'open', failed: [], new_comments: 0, changes_requested: false },
  } as unknown as Continuation
  expect(closeOffer(base)).toEqual({ prominent: false, text: null })
  expect(closeOffer({ ...base, follow: { ...base.follow!, state: 'merged' } })).toEqual({
    prominent: true,
    text: 'PR #149 merged',
  })
  expect(closeOffer({ ...base, follow: null })).toEqual({ prominent: false, text: null })
  expect(closeOffer({ ...base, actions: { ...base.actions, close: false } })).toBe(null)
})
