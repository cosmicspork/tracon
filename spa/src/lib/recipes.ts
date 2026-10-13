import type { ChannelInfo, NodeInfo, RecipeOffer } from './types'

export interface OfferTarget {
  channel: string
  peers: { id: string; name: string }[]
}

/**
 * The channels a recipe can be offered on, and to whom: an ordinary channel
 * this node is in, with at least one other member. A recipe is offered only
 * to channel co-members.
 */
export function offerTargets(channels: ChannelInfo[], nodes: NodeInfo[], self: string): OfferTarget[] {
  const name = (id: string) => nodes.find((node) => node.id === id)?.name || id.slice(0, 12)
  return channels
    .filter((channel) => !channel.name.startsWith('@') && !channel.archived && channel.nodes.includes(self))
    .map((channel) => ({
      channel: channel.name,
      peers: channel.nodes.filter((id) => id !== self).map((id) => ({ id, name: name(id) })),
    }))
    .filter((target) => target.peers.length > 0)
}

/** The forge identity a `[[repo]]` path names by itself (`github.com/owner/name`), if it does. */
export function forgeIdentity(path: string): string | null {
  const segments = path.split('/')
  if (segments.length < 3 || segments.length > 8) return null
  const [host, ...rest] = segments
  const hostOk = host.includes('.') && host.split('.').every((label) => /^[a-z0-9-]+$/.test(label))
  const restOk = rest.every((part) => /^[A-Za-z0-9._-]+$/.test(part) && !part.startsWith('.'))
  return hostOk && restOk ? path : null
}

/** A field's value as the comparison shows it. */
export function shown(value: unknown): string {
  if (value === null || value === undefined) return 'not set'
  if (Array.isArray(value)) return value.length ? value.join('\n') : 'none'
  if (typeof value === 'boolean') return value ? 'yes' : 'no'
  return String(value)
}

/** Undecided offers first, then the newest. */
export function inboxOrder(offers: RecipeOffer[]): RecipeOffer[] {
  return [...offers].sort(
    (a, b) => Number(b.state === 'pending') - Number(a.state === 'pending') || b.received_ms - a.received_ms,
  )
}

/** A result line for one node an offer went to. */
export function offerResultLine(result: { name: string; node: string; state: string; detail?: string }): string {
  const who = result.name || result.node.slice(0, 12)
  switch (result.state) {
    case 'offered':
      return `${who}: offered; its operator decides`
    case 'queued':
      return `${who}: waiting in the outbox until it is back`
    default:
      return `${who}: refused${result.detail ? ` (${result.detail})` : ''}`
  }
}
