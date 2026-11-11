// How a provider's reported capabilities read in the checks list.

import type { CapabilityLine, Elsewhere } from './types'

const NAMES: Record<string, string> = {
  runtime_socket: 'runtime socket',
  image_identity: 'image identity',
}

/** `runtime_socket` → `runtime socket`. */
export function capabilityName(capability: string): string {
  return NAMES[capability] ?? capability
}

/**
 * The chip a line wears: `off` for one its check did not establish, `warn`
 * for a value that is itself a caution (a route nobody verified, an open
 * resolver, a reachable runtime socket), plain otherwise.
 */
export function capabilityTone(line: CapabilityLine): 'off' | 'warn' | '' {
  if (line.value === null) return 'off'
  if (['unverified', 'provider-open', 'present'].includes(line.value)) return 'warn'
  return ''
}

/** One line of what a provider elsewhere stands at, in the checks list. */
export interface ElsewhereLine {
  label: string
  tone: 'off' | 'warn' | 'bad' | ''
  text: string
  detail?: string
}

const REACH: Record<string, string> = {
  address: "the node's own address, as TLS",
  'hub relay': "the hub relay's listener, as TLS",
  'hub url': "the hub's own URL, as TLS in a WebSocket",
}

/**
 * A provider elsewhere as lines beside its capabilities: whether a session
 * can go there now and why not, how its sandboxes reach the node, when
 * placement sends sessions there and how many run, and which catalogue
 * services run in its sandboxes.
 */
export function elsewhereLines(e: Elsewhere): ElsewhereLine[] {
  const limit = e.capacity.limit === null ? '' : ` of ${e.capacity.limit}`
  const lines: ElsewhereLine[] = [
    e.healthy
      ? { label: 'health', tone: '', text: 'can take a session' }
      : { label: 'health', tone: 'bad', text: 'unavailable', detail: e.why ?? 'no reason given' },
    {
      label: 'reach',
      tone: e.reach === 'hub url' ? 'warn' : '',
      text: `${e.reach} · ${e.dial}`,
      detail: `${REACH[e.reach] ?? e.reach}; egress admits ${e.egress.join(', ') || 'nothing'}`,
    },
    {
      label: 'placement',
      tone: e.placement === 'off' ? 'off' : '',
      text: `${e.placement} · ${e.capacity.running}${limit} running`,
    },
  ]
  for (const s of e.services) {
    lines.push(
      s.ok
        ? { label: `service ${s.name}`, tone: '', text: 'runs inside' }
        : { label: `service ${s.name}`, tone: 'off', text: 'refused', detail: s.why },
    )
  }
  return lines
}
