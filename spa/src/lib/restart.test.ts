import { expect, test } from 'bun:test'
import { keyList, restartBanner } from './restart'
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

test('a file that does not load and a restart that failed are critical', () => {
  expect(restartBanner(status({ config: { boot_hash: 'a', hash: 'b', keys: [], unparseable: true } }))?.tone).toBe('crit')
  const failed = restartBanner(status({ last_error: 'gave up waiting' }))
  expect(failed?.tone).toBe('crit')
  expect(failed?.detail).toBe('gave up waiting.')
})

test('a long list of keys is cut', () => {
  expect(keyList(['a', 'b', 'c', 'd', 'e', 'f'])).toBe('a, b, c, d and 2 more')
})
