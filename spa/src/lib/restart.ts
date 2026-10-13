// The restart banner: what a restart of this node would change, and the one
// asked for. The node computes the facts (`GET /api/restart`); this decides
// what a person is told and offered.

import type { RestartStatus } from './types'

export interface RestartBannerView {
  /** `crit` when a restart would fail or did not happen. */
  tone: 'warn' | 'crit'
  headline: string
  detail: string
  /** Offer to restart: something is owed and a supervisor would start it again. */
  offer: boolean
  /** Offer to withdraw a restart that is still waiting. */
  withdraw: boolean
}

const SHOWN_KEYS = 4

/** `a, b, c` or `a, b, c, d and 3 more`. */
export function keyList(keys: string[]): string {
  const shown = keys.slice(0, SHOWN_KEYS).join(', ')
  return keys.length > SHOWN_KEYS ? `${shown} and ${keys.length - SHOWN_KEYS} more` : shown
}

function count(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`
}

export function restartBanner(s: RestartStatus | null): RestartBannerView | null {
  if (!s) return null
  if (s.plan?.state === 'restarting') {
    return {
      tone: 'warn',
      headline: 'Restarting',
      detail: 'the node is stopping to start again; this page reconnects when it is back.',
      offer: false,
      withdraw: false,
    }
  }
  if (s.plan) {
    const waiting = [
      s.busy.turns.length ? count(s.busy.turns.length, 'turn', 'turns') : '',
      s.busy.checks ? count(s.busy.checks, 'check', 'checks') : '',
    ].filter(Boolean)
    return {
      tone: 'warn',
      headline: 'Restarting when idle',
      detail: `${waiting.length ? `waiting for ${waiting.join(' and ')} to finish` : 'about to restart'}; new sessions and turns wait until the node is back.`,
      offer: false,
      withdraw: true,
    }
  }
  const owed = s.config?.keys ?? []
  if (s.config?.unparseable) {
    return {
      tone: 'crit',
      headline: 'node.toml does not load',
      detail: 'a restart would not start the node; fix the file first.',
      offer: false,
      withdraw: false,
    }
  }
  if (s.last_error && !owed.length) {
    return { tone: 'crit', headline: 'The restart did not happen', detail: `${s.last_error}.`, offer: false, withdraw: false }
  }
  if (!owed.length) return null
  const why = s.last_error ? ` The last restart did not happen: ${s.last_error}.` : ''
  return {
    tone: s.last_error ? 'crit' : 'warn',
    headline: 'node.toml changed',
    detail: s.restart.available
      ? `restart to apply ${keyList(owed)}.${why}`
      : `restart to apply ${keyList(owed)}; this node cannot restart itself (${s.restart.reason ?? 'no supervisor'}).${why}`,
    offer: s.restart.available,
    withdraw: false,
  }
}
