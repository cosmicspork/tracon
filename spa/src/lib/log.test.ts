import { expect, test } from 'bun:test'
import { groupLog, groupOpen, groupSummary } from './log'
import type { Event } from './types'

let seq = 0
function ev(kind: string, ref_id: string | null = null, payload: Record<string, unknown> = {}): Event {
  seq += 1
  return { seq, session_id: 's', kind, ref_id, payload, at_ms: 0, mono_ms: seq }
}

test('consecutive tool calls fold into one group', () => {
  const log = groupLog([
    ev('user_prompt'),
    ev('tool_call', 'a', { kind: 'read' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'read' }),
    ev('tool_result', 'b', { status: 'completed' }),
    ev('message'),
  ])
  expect(log.map((l) => l.kind)).toEqual(['leaf', 'tools', 'leaf'])
  expect(log[1].tools!.length).toBe(2)
  expect(groupOpen(log[1].tools!)).toBe(false)
})

test('a group stays open while a call lacks its result', () => {
  const log = groupLog([ev('tool_call', 'a', { kind: 'execute' })])
  expect(groupOpen(log[0].tools!)).toBe(true)
})

test('a permission request breaks the group at its true position', () => {
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'execute' }),
    ev('permission_request', 'p1'),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'execute' }),
  ])
  expect(log.map((l) => l.kind)).toEqual(['tools', 'leaf', 'tools'])
  // The result still lands on its call even across the break.
  expect(log[0].tools![0].result).toBeDefined()
})

test('summary counts by kind and reports failures', () => {
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'read' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'read' }),
    ev('tool_result', 'b', { status: 'completed' }),
    ev('tool_call', 'c', { kind: 'execute' }),
    ev('tool_result', 'c', { status: 'failed' }),
  ])
  expect(groupSummary(log[0].tools!)).toBe('Read 2 files, ran 1 shell command · 1 failed')
})

test('an orphan tool_result is kept as a leaf', () => {
  const log = groupLog([ev('tool_result', 'ghost', { status: 'completed' })])
  expect(log[0].kind).toBe('leaf')
})
