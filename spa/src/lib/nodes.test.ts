import { expect, test } from 'bun:test'
import { eligibleNodes, hubBanner, nodeLabel, unreachableReason, upsertNode } from './nodes'
import type { MeshState, NodeInfo } from './types'

function node(over: Partial<NodeInfo>): NodeInfo {
  return {
    id: 'x',
    name: 'x',
    state: 'ready',
    failed_check: null,
    failed_detail: null,
    harness: { id: 'omp', pinned: '1', found: '1', mismatch: false },
    models: [],
    checked_at_ms: null,
    is_self: false,
    reachable: true,
    last_seen_ms: null,
    ...over,
  }
}

const connected: MeshState = {
  hub: { state: 'connected' },
  hub_url: 'https://hub',
  node_id: 'me',
  fingerprint: null,
  last_ok_ms: null,
  queued: 0,
  delivered_since_reconnect: 0,
  undecryptable: 0,
  last_error: null,
  last_refusal: null,
}

test('self sorts first, then by name; upsert replaces', () => {
  let list = upsertNode([], node({ id: 'z', name: 'zeta' }))
  list = upsertNode(list, node({ id: 'me', name: 'mine', is_self: true }))
  list = upsertNode(list, node({ id: 'a', name: 'alpha' }))
  expect(list.map((n) => n.id)).toEqual(['me', 'a', 'z'])
  list = upsertNode(list, node({ id: 'z', name: 'zeta', reachable: false }))
  expect(list.length).toBe(3)
  expect(list[2].reachable).toBe(false)
})

test('reasons follow hub and peer state', () => {
  const nodes = [node({ id: 'me', is_self: true }), node({ id: 'p', name: 'peer' }), node({ id: 'q', name: 'gone', reachable: false })]
  expect(unreachableReason(nodes, connected, 'me')).toBeNull()
  expect(unreachableReason(nodes, connected, 'p')).toBeNull()
  expect(unreachableReason(nodes, connected, 'q')).toBe('node unreachable')
  expect(unreachableReason(nodes, { ...connected, hub: { state: 'unreachable', since_ms: 1 } }, 'p')).toBe('hub unreachable')
  expect(unreachableReason(nodes, { ...connected, hub: { state: 'unreachable', since_ms: 1 } }, 'me')).toBeNull()
  expect(unreachableReason(nodes, connected, 'nope')).toBe('node unknown')
  expect(nodeLabel(nodes, 'p')).toBe('peer')
  expect(nodeLabel(nodes, 'abcdefghij')).toBe('abcdefgh')
})

test('eligible nodes respect bindings, readiness, and reach', () => {
  const nodes = [
    node({ id: 'me', is_self: true }),
    node({ id: 'p' }),
    node({ id: 'r', state: 'refused' }),
    node({ id: 'u', reachable: false }),
  ]
  expect(eligibleNodes(nodes, {}, 'personal').map((n) => n.id)).toEqual(['me', 'p'])
  expect(eligibleNodes(nodes, { personal: ['p', 'u'] }, 'personal').map((n) => n.id)).toEqual(['p'])
  expect(hubBanner(connected)).toBeNull()
  expect(hubBanner({ ...connected, hub: { state: 'unreachable', since_ms: 1 } })).toBe('hub unreachable')
  expect(hubBanner(null)).toBeNull()
})
