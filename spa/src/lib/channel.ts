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
