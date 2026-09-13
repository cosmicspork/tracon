// Pure helpers over the node list: which node a thing belongs to and whether
// the operator can act on it from here. Kept free of Svelte so they test flat.

import type { MeshState, ModelOption, NodeInfo, ProviderInfo } from './types'

/**
 * Upsert a node into the list, self first, then by name. `loopback` is a fact
 * about how this client reached the node, answered only by the request that
 * asked; a `node` frame from the stream does not carry it, so the value
 * already held is kept rather than dropped.
 */
export function upsertNode(nodes: NodeInfo[], node: NodeInfo): NodeInfo[] {
  const prev = nodes.find((n) => n.id === node.id)
  const next = nodes.filter((n) => n.id !== node.id)
  next.push(node.loopback === undefined && prev?.loopback !== undefined ? { ...node, loopback: prev.loopback } : node)
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
 * How a node is named on a chip that belongs to something else — a session, a
 * review, a permission. This node is "you": its hostname is the one name the
 * operator never needs told, and it is usually the longest.
 */
export function chipLabel(nodes: NodeInfo[], id: string): string {
  return nodeById(nodes, id)?.is_self ? 'you' : nodeLabel(nodes, id)
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

/**
 * The readiness an operator can rely on before sending a new task to a node.
 * This intentionally says nothing about channel membership: that is a
 * separate scope check made by `eligibleNodes`.
 */
export function nodeReadiness(node: NodeInfo): { canRun: boolean; label: string; detail: string } {
  if (!node.reachable) {
    return { canRun: false, label: 'Unreachable', detail: 'This node is not presently reachable through the mesh.' }
  }
  if (node.state === 'unknown') {
    return {
      canRun: false,
      label: 'Isolation unknown',
      detail: 'This node has not reported whether its isolated runtime is ready.',
    }
  }
  if (node.state === 'refused') {
    const failure = [node.failed_check, node.failed_detail].filter((part): part is string => Boolean(part)).join(': ')
    return {
      canRun: false,
      label: 'Isolation refused',
      detail: failure ? `The isolated runtime refused this node: ${failure}.` : 'The isolated runtime refused this node.',
    }
  }
  if (node.harness.mismatch) {
    return {
      canRun: false,
      label: 'Runtime mismatch',
      detail: `This node expects ${node.harness.pinned}, but found ${node.harness.found ?? 'no runtime'}.`,
    }
  }
  if (node.models.length === 0) {
    return {
      canRun: false,
      label: 'No model offered',
      detail: 'This ready runtime has not offered a model for new sessions.',
    }
  }
  return {
    canRun: true,
    label: 'Ready',
    detail: `${node.models.length} model${node.models.length === 1 ? '' : 's'} offered by this ready isolated runtime.`,
  }
}

/**
 * The models a node can actually use for a channel. A node-wide probe is not
 * enough: its provider must still be connected and bound to that channel.
 * Peer summaries may omit providers on older nodes; absence is not evidence
 * that a model can run there, so it stays unavailable.
 */
export function modelsForChannel(
  node: NodeInfo,
  channel: string,
  localProviders?: ProviderInfo[],
  bindings?: Record<string, unknown>,
): ModelOption[] {
  const providers = node.is_self ? (localProviders ?? node.providers) : node.providers
  if (!providers?.length) return []
  const allowed = Array.isArray(bindings?.providers) ? bindings.providers : null
  if (!providers.some((provider) => provider.state === 'connected'
    && provider.channels?.includes(channel)
    && (allowed === null || allowed.includes(provider.name)))) return []
  return node.models.filter((model) => {
    const slash = model.value.indexOf('/')
    const name = slash < 0 ? null : model.value.slice(0, slash)
    const provider = name === null ? undefined : providers.find((candidate) => candidate.name === name)
    // Adapter-owned aliases cannot be reverse-engineered. Named providers,
    // however, must have their own usable credential, not another provider's.
    return provider === undefined || (provider.state === 'connected'
      && provider.channels?.includes(channel)
      && (allowed === null || allowed.includes(provider.name)))
  })
}

/** The nodes that could run a session on `channel`: members with a usable runtime. */
export function eligibleNodes(nodes: NodeInfo[], channels: Record<string, string[]>, channel: string): NodeInfo[] {
  const bound = channels[channel]
  return nodes.filter((node) => (!bound || bound.includes(node.id)) && nodeReadiness(node).canRun)
}

export function hubBanner(mesh: MeshState | null): string | null {
  if (!mesh || mesh.hub.state !== 'unreachable') return null
  return 'hub unreachable'
}
