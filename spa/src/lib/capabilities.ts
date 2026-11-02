// How a provider's reported capabilities read in the checks list.

import type { CapabilityLine } from './types'

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
