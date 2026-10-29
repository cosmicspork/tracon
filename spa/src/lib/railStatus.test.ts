import { expect, test } from 'bun:test'
import { hubLight, nodeLight, runningHere } from './railStatus'
import type { AwakeState, MeshState, NodeInfo, Session } from './types'

const now = 10_000_000
const self = { id: 'me', name: 'bazzite', state: 'ready' } as NodeInfo
const base = { connected: true, node: self, cannotRun: false, waiting: 0, running: 0, awake: null, now }

test('broken outranks waiting, waiting outranks running, running outranks idle', () => {
  expect(nodeLight(base)).toMatchObject({ tone: 'idle', state: 'idle' })
  expect(nodeLight({ ...base, running: 2 })).toMatchObject({ tone: 'ok', state: '2 sessions running' })
  expect(nodeLight({ ...base, running: 2, waiting: 1 })).toMatchObject({ tone: 'wait', state: '1 waiting on you' })
  expect(nodeLight({ ...base, waiting: 1, cannotRun: true }).tone).toBe('crit')
  expect(nodeLight({ ...base, node: { ...self, state: 'refused' } }).state).toBe('failed its isolation check')
  expect(nodeLight({ ...base, waiting: 1, connected: false })).toMatchObject({ tone: 'crit', state: 'disconnected' })
})

test('keep-awake and a recent sleep go to the tooltip, an old sleep does not', () => {
  const awake: AwakeState = { held: true, reason: '1 session working', method: 'logind', error: null, last_suspend: { woke_ms: now - 60_000, asleep_ms: 67 * 60_000 } }
  expect(nodeLight({ ...base, awake }).detail).toEqual(['bazzite', 'keeping awake: 1 session working', 'slept 67 min, woke 1m ago'])
  const old = { ...awake, held: false, error: 'no logind', last_suspend: { woke_ms: now - 3_600_000, asleep_ms: 1 } }
  expect(nodeLight({ ...base, awake: old }).detail).toEqual(['bazzite', 'cannot keep this machine awake: no logind'])
})

test('running counts working sessions on this node only', () => {
  const s = (over: Partial<Session>) => ({ node_id: 'me', state: 'running', archived_ms: null, ...over }) as Session
  const sessions = [s({}), s({ state: 'starting' }), s({ state: 'waiting_on_check' }), s({ state: 'closed' }), s({ state: 'waiting_on_you' }), s({ node_id: 'peer' }), s({ archived_ms: 1 })]
  expect(runningHere(sessions, 'me')).toBe(3)
  expect(runningHere(sessions, undefined)).toBe(0)
})

test('a hub line shows only when a hub is configured', () => {
  const mesh = (hub: MeshState['hub']) => ({ hub }) as MeshState
  expect(hubLight(null, now)).toBeNull()
  expect(hubLight(mesh({ state: 'disabled' }), now)).toBeNull()
  expect(hubLight(mesh({ state: 'connected' }), now)).toMatchObject({ tone: 'ok', state: 'connected' })
  expect(hubLight(mesh({ state: 'unreachable', since_ms: now - 300_000 }), now)).toMatchObject({ tone: 'crit', state: 'unreachable for 5m' })
})
