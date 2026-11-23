import { expect, test } from 'bun:test'

import { ApiError, nodePath } from './api'
import { externalStopped, nodeChoices, notifyEnabled, peerFailure } from './peer-admin'
import type { NodeInfo } from './types'

const nodes = [
  { id: 'self', name: 'bazzite', is_self: true, reachable: true },
  { id: 'peer-b', name: 'homelab', is_self: false, reachable: true },
  { id: 'peer-a', name: 'alpha', is_self: false, reachable: false },
] as NodeInfo[]

test('a path runs on this node, or through the mesh on a peer, with no query', () => {
  expect(nodePath('', '/api/plugins')).toBe('/api/plugins')
  expect(nodePath(undefined, '/api/admin/restart')).toBe('/api/admin/restart')
  expect(nodePath('peer-b', '/api/plugins/hora/rollback')).toBe('/api/nodes/peer-b/admin/plugins/hora/rollback')
  expect(nodePath('a/b', '/api/restart')).toBe('/api/nodes/a%2Fb/admin/restart')
})

test('this node is picked first, then the peers by name, saying which are away', () => {
  expect(nodeChoices(nodes)).toEqual([
    { id: '', label: 'bazzite (this node)' },
    { id: 'peer-a', label: 'alpha (unreachable)' },
    { id: 'peer-b', label: 'homelab' },
  ])
  expect(nodeChoices([])).toEqual([{ id: '', label: 'This node' }])
})

test('a peer that does not name this node says what to change', () => {
  const refused = new ApiError(403, 'homelab does not name bazzite', undefined, undefined, 'not_admin_peer')
  expect(peerFailure(nodes, 'peer-b', refused)).toBe(
    'homelab must list this node in [mesh] admin_peers to be administered from here. homelab does not name bazzite',
  )
  // Any other refusal, or one about this node, is shown as it came.
  expect(peerFailure(nodes, 'peer-b', new ApiError(409, 'busy', undefined, undefined, 'peer_refused'))).toBe('busy')
  expect(peerFailure(nodes, '', refused)).toBe('homelab does not name bazzite')
  expect(peerFailure(nodes, 'peer-b', 'plain')).toBe('plain')
})

test('a channel binding reads as the node keeps it', () => {
  expect(notifyEnabled({})).toBe(true)
  expect(notifyEnabled({ notify: { enabled: false } })).toBe(false)
  expect(notifyEnabled({ notify: { sink: 'tray' } })).toBe(false)
  expect(externalStopped({ external_paused: true })).toBe(true)
  expect(externalStopped({ external_stopped: false })).toBe(false)
  expect(externalStopped(undefined)).toBe(false)
})
