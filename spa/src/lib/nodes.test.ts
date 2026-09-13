import { expect, test } from 'bun:test'
import { eligibleNodes, hubBanner, modelsForChannel, nodeLabel, unreachableReason, upsertNode } from './nodes'
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
  held: 0,
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


test('channel models require a connected provider bound to that channel', () => {
  const providers = [
    {
      name: 'openai',
      state: 'connected' as const,
      kind: 'api_key' as const,
      can_login: false,
      identity: null,
      expires_ms: null,
      channels: ['work'],
      updated_ms: null,
    },
  ]
  const peer = node({ models: [{ value: 'm', name: 'm' }], providers })
  expect(modelsForChannel(peer, 'work')).toEqual(peer.models)
  expect(modelsForChannel(peer, 'personal')).toEqual([])
  expect(modelsForChannel(node({ is_self: true, models: peer.models }), 'work', providers)).toEqual(peer.models)
  expect(modelsForChannel(node({ models: peer.models, providers: [{ ...providers[0], state: 'disconnected' }] }), 'work')).toEqual([])
})

test('a model cannot borrow another provider’s channel access', () => {
  const peer = node({
    models: [
      { value: 'openai/work-model', name: 'Work model' },
      { value: 'anthropic/personal-model', name: 'Personal model' },
    ],
    providers: [
      { name: 'openai', state: 'connected', kind: 'api_key', can_login: false, identity: null, expires_ms: null, channels: ['work'], updated_ms: null },
      { name: 'anthropic', state: 'connected', kind: 'api_key', can_login: false, identity: null, expires_ms: null, channels: ['personal'], updated_ms: null },
    ],
  })
  expect(modelsForChannel(peer, 'work').map((model) => model.value)).toEqual(['openai/work-model'])
  expect(modelsForChannel(peer, 'personal').map((model) => model.value)).toEqual(['anthropic/personal-model'])
  expect(modelsForChannel(peer, 'work', undefined, { providers: ['anthropic'] })).toEqual([])
  peer.providers![0].state = 'disconnected'
  expect(modelsForChannel(peer, 'work')).toEqual([])
})

test('legacy provider summaries cannot claim channel-scoped model access', () => {
  const peer = node({
    models: [{ value: 'anthropic/model', name: 'Model' }],
    providers: JSON.parse('[{"name":"anthropic","state":"connected"}]'),
  })
  expect(modelsForChannel(peer, 'work')).toEqual([])
})

test('eligible nodes respect membership and every readiness guard', () => {
  const nodes = [
    node({ id: 'me', is_self: true, models: [{ value: 'm', name: 'm' }] }),
    node({ id: 'p', models: [{ value: 'm', name: 'm' }] }),
    node({ id: 'r', state: 'refused', models: [{ value: 'm', name: 'm' }] }),
    node({ id: 'u', reachable: false, models: [{ value: 'm', name: 'm' }] }),
    node({ id: 'x', harness: { id: 'omp', pinned: '2', found: '1', mismatch: true }, models: [{ value: 'm', name: 'm' }] }),
    node({ id: 'empty' }),
  ]
  expect(eligibleNodes(nodes, {}, 'personal').map((n) => n.id)).toEqual(['me', 'p'])
  expect(eligibleNodes(nodes, { personal: ['p', 'u'] }, 'personal').map((n) => n.id)).toEqual(['p'])
  expect(hubBanner(connected)).toBeNull()
  expect(hubBanner({ ...connected, hub: { state: 'unreachable', since_ms: 1 } })).toBe('hub unreachable')
  expect(hubBanner(null)).toBeNull()
})

test('a node frame without loopback keeps the flag the request answered with', () => {
  let list = upsertNode([], node({ id: 'me', name: 'mine', is_self: true, loopback: true }))
  list = upsertNode(list, node({ id: 'me', name: 'mine', is_self: true, models: [{ value: 'm', name: 'm' }] }))
  expect(list[0].loopback).toBe(true)
  expect(list[0].models.length).toBe(1)
  // An answer that says otherwise is still believed.
  list = upsertNode(list, node({ id: 'me', name: 'mine', is_self: true, loopback: false }))
  expect(list[0].loopback).toBe(false)
})
