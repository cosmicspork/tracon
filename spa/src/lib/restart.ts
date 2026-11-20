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

/** `restart.mechanism` for a container its deployment restarts on exit (`TRACON_RESTART_BY_EXIT=1`). */
export const EXIT_MECHANISM = 'exit for the container to be restarted'

const SHOWN_KEYS = 4

/** `a, b, c` or `a, b, c, d and 3 more`. */
export function keyList(keys: string[]): string {
  const shown = keys.slice(0, SHOWN_KEYS).join(', ')
  return keys.length > SHOWN_KEYS ? `${shown} and ${keys.length - SHOWN_KEYS} more` : shown
}

/** `a, b and c`. */
export function list(items: string[]): string {
  return items.length > 1 ? `${items.slice(0, -1).join(', ')} and ${items[items.length - 1]}` : items.join('')
}

function count(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`
}

/** What a `when_idle` restart waits for, as `2 turns`, `1 check`, …: empty when nothing runs. */
export function busyList(s: RestartStatus): string[] {
  return [
    s.busy.turns.length ? count(s.busy.turns.length, 'turn', 'turns') : '',
    s.busy.checks ? count(s.busy.checks, 'check', 'checks') : '',
    s.busy.reviews ? count(s.busy.reviews, 'review', 'reviews') : '',
    s.busy.plugins?.length ? count(s.busy.plugins.length, 'plugin instance', 'plugin instances') : '',
  ].filter(Boolean)
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
    const waiting = busyList(s)
    return {
      tone: 'warn',
      headline: 'Restarting when idle',
      detail: `${waiting.length ? `waiting for ${list(waiting)} to finish` : 'about to restart'}; new sessions and turns wait until the node is back.`,
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

/** What one "Restart" click does, decided against a status read just before it. */
export type RestartAsk =
  /** Nothing would start the node again, or there is no file to restart into. */
  | { kind: 'unavailable'; reason: string }
  /** Nothing runs and no session is live: restart at once. */
  | { kind: 'direct' }
  /** Something runs or a session is live: say what, and offer the ways on. */
  | {
      kind: 'confirm'
      /** What a `when_idle` restart would wait for (`busyList`). */
      running: string[]
      /** The live sessions a restart ends. */
      sessions: RestartStatus['closing']
      /** Waiting for the running work is an option: something is running. */
      canWait: boolean
    }

export function restartAsk(s: RestartStatus): RestartAsk {
  if (!s.config) return { kind: 'unavailable', reason: 'this node did not record the node.toml it started from' }
  if (s.config.unparseable) return { kind: 'unavailable', reason: 'node.toml does not load' }
  if (!s.restart.available) return { kind: 'unavailable', reason: s.restart.reason ?? 'nothing would start this node again' }
  const running = busyList(s)
  if (!running.length && !s.closing.length) return { kind: 'direct' }
  return { kind: 'confirm', running, sessions: s.closing, canWait: running.length > 0 }
}

export interface RestartCardView {
  /** How this node restarts, or null where it cannot. */
  mechanism: string | null
  /** What a restart applies. */
  applies: string
  /** The restart waiting or under way, in the banner's words; null when none. */
  plan: string | null
  /** Why the last restart did not happen. */
  lastError: string | null
}

export function restartCard(s: RestartStatus): RestartCardView {
  const owed = s.config?.keys ?? []
  const applies = !s.config
    ? 'this node did not record the node.toml it started from.'
    : s.config.unparseable
      ? 'node.toml does not load; a restart would not start the node, so fix the file first.'
      : owed.length
        ? `applies ${keyList(owed)}, changed in node.toml since the node started.`
        : 'no change is waiting; the node starts again on the same node.toml.'
  const banner = s.plan ? restartBanner(s) : null
  return {
    mechanism: s.restart.available ? s.restart.mechanism : null,
    applies,
    plan: banner ? `${banner.headline}: ${banner.detail}` : null,
    lastError: s.last_error,
  }
}
