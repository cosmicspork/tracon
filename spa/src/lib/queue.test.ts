import { expect, test } from 'bun:test'
import { applySessionFrame, isTerminalState } from './queue'
import type { Queue, Session } from './types'

const session = (id: string, state: string, archived_ms: number | null = null) =>
  ({ id, state, archived_ms, created_ms: 1, branch: `feat/${id}` }) as unknown as Session

const empty: Queue = { waiting: [], reviews: [], promotions: [], running: [], ended: [] }

test('the states that count as over', () => {
  expect(['closed', 'killed_budget', 'failed'].every(isTerminalState)).toBe(true)
  expect(['running', 'starting', 'waiting_on_you'].some(isTerminalState)).toBe(false)
})

test('a session that ends moves from running to ended', () => {
  const q = applySessionFrame({ ...empty, running: [session('a', 'running')] }, session('a', 'closed'))
  expect(q.running).toEqual([])
  expect(q.ended.map((s) => s.id)).toEqual(['a'])
})

test('a running session is updated in place, not duplicated', () => {
  const q = applySessionFrame(
    { ...empty, running: [session('a', 'starting'), session('b', 'running')] },
    session('a', 'running'),
  )
  expect(q.running.map((s) => s.id)).toEqual(['a', 'b'])
  expect(q.running[0].state).toBe('running')
})

test('an archived session is on neither list', () => {
  const q = applySessionFrame({ ...empty, ended: [session('a', 'closed')] }, session('a', 'closed', 1700))
  expect(q.ended).toEqual([])
  expect(q.running).toEqual([])
})

test('archiving does not disturb what is waiting', () => {
  const waiting = [{ id: 'p1' }] as unknown as Queue['waiting']
  const q = applySessionFrame({ ...empty, waiting, ended: [session('a', 'closed')] }, session('a', 'closed', 1))
  expect(q.waiting).toBe(waiting)
})

test('a session brought back lands where its state says', () => {
  const q = applySessionFrame(empty, session('a', 'closed'))
  expect(q.ended.map((s) => s.id)).toEqual(['a'])
})
