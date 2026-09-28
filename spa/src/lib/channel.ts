// Which channel a screen starts on. The node's own preference wins, because
// the operator set it for every browser; then this browser's last choice;
// then whatever this node ran last; then the first name there is. An
// archived or vanished channel is skipped at every step.

const KEY = 'tracon.channel'

export function rememberedChannel(): string | null {
  try {
    return localStorage.getItem(KEY)
  } catch {
    return null
  }
}

export function rememberChannel(name: string): void {
  try {
    localStorage.setItem(KEY, name)
  } catch {
    // Storage can be missing or refused; the node's default still applies.
  }
}

export function defaultChannel(o: {
  names: string[]
  remembered?: string | null
  nodeDefault?: string | null
  sessions?: { channel: string; created_ms: number }[]
}): string {
  const open = new Set(o.names)
  if (o.nodeDefault && open.has(o.nodeDefault)) return o.nodeDefault
  if (o.remembered && open.has(o.remembered)) return o.remembered
  const last = [...(o.sessions ?? [])]
    .sort((a, b) => b.created_ms - a.created_ms)
    .find((s) => open.has(s.channel))
  return last?.channel ?? o.names[0] ?? ''
}

// Which channels a provider sign-in on `nodeId` starts out serving. A
// reconnect keeps what it served; otherwise the node's preferred channel,
// else every open channel the node belongs to.
export function providerChannelSeed(o: {
  existing?: string[]
  nodeDefault?: string | null
  channels: { name: string; nodes: string[]; archived?: number | null }[]
  nodeId: string
}): string[] {
  const open = o.channels.filter((c) => !c.archived)
  const member = open.filter((c) => c.nodes.includes(o.nodeId)).map((c) => c.name)
  const choices = member.length ? member : open.map((c) => c.name)
  const kept = (o.existing ?? []).filter((name) => choices.includes(name))
  if (kept.length) return kept
  if (o.nodeDefault && choices.includes(o.nodeDefault)) return [o.nodeDefault]
  return choices
}
