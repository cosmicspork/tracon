import { expect, test } from 'bun:test'
import { appendPage, historyQuery, repoLabel, sourceLabel, verdictLabel } from './reviewHistory'
import type { DecidedReview } from './types'

function row(o: Partial<DecidedReview> = {}): DecidedReview {
  return {
    id: 'r1',
    node_id: 'n1',
    held_here: true,
    channel: 'personal',
    session_id: null,
    lane: 'laptop',
    title: 'Clamp the refill',
    provider: 'github',
    project: 'o/orbit',
    branch: 'feat/x',
    state: 'approved',
    publish_result: null,
    decided_ms: 1,
    ...o,
  }
}

test('the query asks for decided reviews, narrowed as filtered, after a page', () => {
  expect(historyQuery({ external: false, channel: '' })).toBe('state=decided')
  expect(historyQuery({ external: true, channel: 'work' }, '50:r9')).toBe(
    'state=decided&source=external&channel=work&before=50%3Ar9',
  )
})

test('a row names its verdict, repository and source', () => {
  expect(verdictLabel('approved')).toBe('Approved')
  expect(verdictLabel('rejected')).toBe('Rejected')
  expect(repoLabel(row())).toBe('o/orbit')
  expect(repoLabel(row({ project: null }))).toBe('github')
  expect(sourceLabel(row())).toBe('your own harness · laptop · personal')
  expect(sourceLabel(row({ lane: null }))).toBe('your own harness · personal')
  expect(sourceLabel(row({ session_id: '0190c8a2-0000-7000-8000-000000000001' }))).toStartWith('session-')
})

test('a later page adds only reviews not shown yet', () => {
  const shown = [row({ id: 'a' }), row({ id: 'b' })]
  expect(appendPage(shown, [row({ id: 'b' }), row({ id: 'c' })]).map((r) => r.id)).toEqual(['a', 'b', 'c'])
})
