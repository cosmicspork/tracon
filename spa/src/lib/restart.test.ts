import { expect, test } from 'bun:test'
import { busyList, keyList, restartAsk, restartBanner, restartCard } from './restart'
import type { RestartStatus } from './types'

function status(over: Partial<RestartStatus> = {}): RestartStatus {
  return {
    config: { boot_hash: 'a', hash: 'a', keys: [], unparseable: false },
    restart: { available: true, mechanism: 'systemd user service', reason: null },
    plan: null,
    busy: { turns: [], checks: 0, reviews: 0 },
    closing: [],
    last_error: null,
    ...over,
  }
}

test('nothing owed says nothing', () => {
  expect(restartBanner(null)).toBeNull()
  expect(restartBanner(status())).toBeNull()
})

test('restart keys that changed are named and a restart is offered', () => {
  const view = restartBanner(status({ config: { boot_hash: 'a', hash: 'b', keys: ['node_name', 'gateway.proxy_port'], unparseable: false } }))
  expect(view?.headline).toBe('node.toml changed')
  expect(view?.detail).toContain('restart to apply node_name, gateway.proxy_port')
  expect(view?.offer).toBe(true)
})

test('a node nothing would start again says why and offers nothing', () => {
  const view = restartBanner(
    status({
      config: { boot_hash: 'a', hash: 'b', keys: ['node_name'], unparseable: false },
      restart: { available: false, mechanism: null, reason: 'not installed' },
    }),
  )
  expect(view?.offer).toBe(false)
  expect(view?.detail).toContain('not installed')
})

test('a waiting restart names what it waits for and can be withdrawn', () => {
  const view = restartBanner(
    status({
      plan: { mode: 'when_idle', requested_ms: 1, deadline_ms: 2, state: 'draining', expected_hash: 'b' },
      busy: { turns: ['s1', 's2'], checks: 1, reviews: 1 },
    }),
  )
  expect(view?.detail).toContain('waiting for 2 turns, 1 check and 1 review')
  expect(view?.withdraw).toBe(true)
  expect(view?.offer).toBe(false)
})

test('a waiting restart counts the plugin instances still running', () => {
  const view = restartBanner(
    status({
      plan: { mode: 'when_idle', requested_ms: 1, deadline_ms: 2, state: 'draining', expected_hash: 'b' },
      busy: { turns: [], checks: 0, reviews: 0, plugins: ['sql/personal'] },
    }),
  )
  expect(view?.detail).toContain('waiting for 1 plugin instance to finish')
})

test('a file that does not load and a restart that failed are critical', () => {
  expect(restartBanner(status({ config: { boot_hash: 'a', hash: 'b', keys: [], unparseable: true } }))?.tone).toBe('crit')
  const failed = restartBanner(status({ last_error: 'gave up waiting' }))
  expect(failed?.tone).toBe('crit')
  expect(failed?.detail).toBe('gave up waiting.')
})

test('a long list of keys is cut', () => {
  expect(keyList(['a', 'b', 'c', 'd', 'e', 'f'])).toBe('a, b, c, d and 2 more')
})

const session = (id: string, turn_active = false): RestartStatus['closing'][number] => ({
  id,
  channel: 'personal',
  repo_path: '/r',
  branch: '',
  state: turn_active ? 'running' : 'idle',
  turn_active,
})

test('one click restarts when nothing runs and no session is live', () => {
  expect(restartAsk(status())).toEqual({ kind: 'direct' })
})

test('live sessions alone are confirmed, with nothing to wait for', () => {
  const ask = restartAsk(status({ closing: [session('s1')] }))
  expect(ask.kind).toBe('confirm')
  if (ask.kind !== 'confirm') return
  expect(ask.canWait).toBe(false)
  expect(ask.running).toEqual([])
  expect(ask.sessions.map((s) => s.id)).toEqual(['s1'])
})

test('anything running can be waited for: a turn, a check, a review, a plugin', () => {
  const cases: RestartStatus['busy'][] = [
    { turns: ['s1'], checks: 0, reviews: 0 },
    { turns: [], checks: 1, reviews: 0 },
    { turns: [], checks: 0, reviews: 1 },
    { turns: [], checks: 0, reviews: 0, plugins: ['sql/personal'] },
  ]
  for (const busy of cases) {
    const ask = restartAsk(status({ busy, closing: busy.turns.length ? [session('s1', true)] : [] }))
    expect(ask.kind).toBe('confirm')
    if (ask.kind === 'confirm') expect(ask.canWait).toBe(true)
  }
  expect(busyList(status({ busy: { turns: ['a', 'b'], checks: 1, reviews: 0 } }))).toEqual(['2 turns', '1 check'])
})

test('a restart that cannot be done says why', () => {
  expect(restartAsk(status({ restart: { available: false, mechanism: null, reason: 'not installed' } }))).toEqual({
    kind: 'unavailable',
    reason: 'not installed',
  })
  const none = restartAsk(status({ config: null }))
  expect(none.kind).toBe('unavailable')
  if (none.kind === 'unavailable') expect(none.reason).toContain('did not record')
  const broken = restartAsk(status({ config: { boot_hash: 'a', hash: 'b', keys: [], unparseable: true } }))
  expect(broken).toEqual({ kind: 'unavailable', reason: 'node.toml does not load' })
})

test('the card names the mechanism and that nothing is waiting', () => {
  const view = restartCard(status({ restart: { available: true, mechanism: 'exit for the container to be restarted', reason: null } }))
  expect(view.mechanism).toBe('exit for the container to be restarted')
  expect(view.applies).toContain('no change is waiting')
  expect(view.plan).toBeNull()
  expect(view.lastError).toBeNull()
  const owed = restartCard(status({ config: { boot_hash: 'a', hash: 'b', keys: ['node_name'], unparseable: false }, last_error: 'gave up' }))
  expect(owed.applies).toContain('node_name')
  expect(owed.lastError).toBe('gave up')
  const waiting = restartCard(
    status({
      plan: { mode: 'when_idle', requested_ms: 1, deadline_ms: 2, state: 'draining', expected_hash: 'a' },
      busy: { turns: ['s1'], checks: 0, reviews: 0 },
    }),
  )
  expect(waiting.plan).toContain('waiting for 1 turn')
})
