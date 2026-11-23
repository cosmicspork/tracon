// Administering a peer from this node's Settings: which nodes a page can act
// on, and what a refusal from one says. A peer runs an action from here only
// if its `[mesh] admin_peers` names this node; everything else about the
// action is the peer's own route's (`nodePath` in api.ts).
import { ApiError } from './api'
import { nodeDisplayName } from './names'
import type { NodeInfo } from './types'

export interface NodeChoice {
  /** Empty for this node, as `nodePath` reads it. */
  id: string
  label: string
}

/** This node first, then each peer, by name, saying which are away. */
export function nodeChoices(nodes: readonly NodeInfo[]): NodeChoice[] {
  const self = nodes.find((node) => node.is_self)
  const peers = nodes
    .filter((node) => !node.is_self)
    .map((node) => ({
      id: node.id,
      label: `${nodeDisplayName(nodes, node.id)}${node.reachable ? '' : ' (unreachable)'}`,
    }))
    .sort((a, b) => a.label.localeCompare(b.label))
  return [{ id: '', label: self ? `${nodeDisplayName(nodes, self.id)} (this node)` : 'This node' }, ...peers]
}

/** What a page shows for a failed action on `node`: for a peer that does
    not name this node in its `admin_peers`, what to change and where. */
export function peerFailure(nodes: readonly NodeInfo[], node: string, cause: unknown): string {
  const message = cause instanceof Error ? cause.message : String(cause)
  if (node && cause instanceof ApiError && cause.kind === 'not_admin_peer') {
    return `${nodeDisplayName(nodes, node)} must list this node in [mesh] admin_peers to be administered from here. ${message}`
  }
  return message
}

/** Whether a channel's members push its events to their devices. */
export function notifyEnabled(bindings: Record<string, unknown>): boolean {
  const notify = (bindings.notify ?? {}) as Record<string, unknown>
  if (typeof notify.enabled === 'boolean') return notify.enabled
  // The former bridge configuration used a tray sink to mean no push.
  return typeof notify.sink === 'string' ? notify.sink !== 'tray' : true
}

/** Whether a node refuses external harnesses on a channel (stopped, or
    paused by its own watch). */
export function externalStopped(bindings: Record<string, unknown> | undefined): boolean {
  return bindings?.external_stopped === true || bindings?.external_paused === true
}
