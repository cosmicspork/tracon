import { nodeById } from './nodes'
import type { NodeInfo, Permission } from './types'

/**
 * A brokered call held for the operator. Nothing waits on it: allowing it
 * makes the node run the call, so it outlives the session that asked and an
 * ended session does not make it moot.
 */
export function isApproval(p: Pick<Permission, 'raw_input'>): boolean {
  if (!p.raw_input) return false
  try {
    return typeof JSON.parse(p.raw_input)?.approval_id === 'string'
  } catch {
    return false
  }
}

/**
 * A session's ask to reach a host. Allowing it opens the host to that
 * session's own grant, for as long as the chosen answer says; it runs nothing,
 * and its arguments are not the operator's to edit.
 */
export function isEgress(p: Pick<Permission, 'raw_input'>): boolean {
  if (!p.raw_input) return false
  try {
    return JSON.parse(p.raw_input)?.tool === 'request_egress'
  } catch {
    return false
  }
}

export interface EditableField {
  key: string
  value: string
}

function toolArguments(p: Pick<Permission, 'kind' | 'raw_input'>): Record<string, unknown> | null {
  if (p.kind !== 'tool' || !p.raw_input) return null
  try {
    const args = JSON.parse(p.raw_input)?.arguments
    return args && typeof args === 'object' && !Array.isArray(args) ? args : null
  } catch {
    return null
  }
}

function isProse(value: unknown): value is string {
  return typeof value === 'string' && (value.length > 40 || value.includes('\n'))
}

// Only the prose arguments of a brokered call are offered for editing; keys,
// slugs and ids stay as the agent sent them and show in the full request.
// A held call is edited on its own page, against what it would change, so
// its card offers nothing to edit; a peer's card cannot open that page, so it
// keeps them.
export function editableFields(p: Pick<Permission, 'kind' | 'raw_input'>, peer = false): EditableField[] {
  if (isApproval(p) && !peer) return []
  const args = toolArguments(p)
  if (!args) return []
  return Object.entries(args)
    .filter((e): e is [string, string] => isProse(e[1]))
    .map(([key, value]) => ({ key, value }))
}

/**
 * The request a card shows under "Full request", or null for none. A held
 * call's page shows it in full, so its card shows none; a peer's card cannot
 * open that page, so it shows the whole request, since nowhere else does.
 */
export function cardRequest(p: Pick<Permission, 'kind' | 'raw_input'>, peer: boolean): string | null {
  if (!p.raw_input) return null
  let input: unknown
  try {
    input = JSON.parse(p.raw_input)
  } catch {
    return p.raw_input
  }
  if (isApproval(p) && !peer) return null
  return JSON.stringify(input, null, 2)
}

export function editedArguments(
  p: Pick<Permission, 'kind' | 'raw_input'>,
  drafts: Record<string, string>,
): Record<string, unknown> | undefined {
  const args = toolArguments(p)
  if (!args) return undefined
  const changed = Object.entries(drafts).filter(([k, v]) => args[k] !== v)
  return changed.length ? { ...args, ...Object.fromEntries(changed) } : undefined
}

/**
 * The answers to this card that are an admin action on the node holding it,
 * which that node confirms only with its own passkey (`admin_answer` on the
 * node): allowing a repository setup proposal or a Dockerfile trial, and
 * saving an egress host to the repository. A peer showing the card cannot
 * confirm them, so it leaves them to that node.
 */
export function stepUpOptions(p: Pick<Permission, 'raw_input'>): string[] {
  if (!p.raw_input) return []
  try {
    const input = JSON.parse(p.raw_input)
    if (typeof input?.approval_id !== 'string') return []
    if (input.tool === 'repo_setup_propose' || input.tool === 'repo_setup_try') return ['allow_once']
    if (input.tool === 'request_egress') return ['allow_repo']
    return []
  } catch {
    return []
  }
}

/**
 * The answers this node leaves to the node holding the card: its step-up
 * answers, unless the card is this node's own. A card whose owner is not
 * known to be this node is treated as a peer's, so an answer it would refuse
 * is never offered. The Home lane and the card both ask this, so a card is
 * never counted as yours while it offers no Allow, or offers one its owner
 * then refuses.
 */
export function withheldOptions(nodes: NodeInfo[], p: Pick<Permission, 'node_id' | 'raw_input'>): string[] {
  return nodeById(nodes, p.node_id)?.is_self ? [] : stepUpOptions(p)
}
