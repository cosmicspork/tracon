// Pure helpers over the node list: which node a thing belongs to and whether
// the operator can act on it from here. Kept free of Svelte so they test flat.

import type { MeshState, NodeInfo } from './types'

/** Upsert a node into the list, self first, then by name. */
export function upsertNode(nodes: NodeInfo[], node: NodeInfo): NodeInfo[] {
  const next = nodes.filter((n) => n.id !== node.id)
  next.push(node)
  return next.sort((a, b) => {
    if (a.is_self !== b.is_self) return a.is_self ? -1 : 1
    return a.name.localeCompare(b.name)
  })
}

export function nodeById(nodes: NodeInfo[], id: string): NodeInfo | undefined {
  return nodes.find((n) => n.id === id)
}

/** A short label for a node id when the node itself is not (yet) known. */
export function nodeLabel(nodes: NodeInfo[], id: string): string {
  return nodeById(nodes, id)?.name || id.slice(0, 8)
}

/**
 * Why a command for something `nodeId` owns cannot be sent right now, or null
 * when it can. Local things are always actionable; a peer must be reachable.
 */
export function unreachableReason(nodes: NodeInfo[], mesh: MeshState | null, nodeId: string): string | null {
  const n = nodeById(nodes, nodeId)
  if (!n) return 'node unknown'
  if (n.is_self) return null
  if (mesh && mesh.hub.state !== 'connected') return 'hub unreachable'
  if (!n.reachable) return 'node unreachable'
  return null
}

/** The nodes that could run a session on `channel`: ready and reachable, bound to it. */
export function eligibleNodes(nodes: NodeInfo[], channels: Record<string, string[]>, channel: string): NodeInfo[] {
  const bound = channels[channel]
  return nodes.filter((n) => (!bound || bound.includes(n.id)) && n.state === 'ready' && !n.harness.mismatch && n.reachable)
}

export function hubBanner(mesh: MeshState | null): string | null {
  if (!mesh || mesh.hub.state !== 'unreachable') return null
  return 'hub unreachable'
}
