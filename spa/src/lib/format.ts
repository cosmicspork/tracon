import type { CodexLimits, LimitWindow } from './types'

/** Token counts as the interface shows them: 15024 → "15k", 1_240_000 → "1.24M". */
export function formatTokens(n: number): string {
  if (n < 1000) return String(n)
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`
  return `${(n / 1_000_000).toFixed(2).replace(/\.?0+$/, '')}M`
}

/** "0.4M/2M", or "0.4M/∞" when a session has no cap. */
export function formatBudget(used: number, budget: number): string {
  return `${formatTokens(used)}/${budget > 0 ? formatTokens(budget) : '∞'}`
}

/**
 * A span of time in one unit: "48s", "12m", "2h", "3d", "2w", "3mo", "1y".
 * Coarsens as it grows, because past a day nobody wants to divide by 24 to
 * find out that "139h" was last Tuesday. Seconds survive at the bottom,
 * where a running session's age is a liveness signal.
 */
export function formatDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h`
  const d = Math.floor(h / 24)
  if (d < 7) return `${d}d`
  const w = Math.floor(d / 7)
  if (w < 5) return `${w}w`
  // Months are 30 days and a year is 365, so the handover is on days: month
  // 12 would otherwise end five days short of a year and report "0y".
  if (d < 365) return `${Math.floor(d / 30)}mo`
  return `${Math.floor(d / 365)}y`
}

/** Age since a unix-ms timestamp. */
export function formatAge(ms: number, now = Date.now()): string {
  return formatDuration(now - ms)
}

/** Time remaining until a deadline: "expires 4m", or "expired". */
export function formatExpiry(expiresMs: number, now = Date.now()): string {
  const s = Math.floor((expiresMs - now) / 1000)
  if (s <= 0) return 'expired'
  if (s < 60) return `expires ${s}s`
  return `expires ${Math.ceil(s / 60)}m`
}

/** "2,000,000" — a whole number with thousands separated, for a field a person types into. */
export function formatGrouped(n: number): string {
  return String(Math.max(0, Math.trunc(n))).replace(/\B(?=(\d{3})+(?!\d))/g, ',')
}

/** The number in a typed field, ignoring separators and anything else that is not a digit. */
export function digits(s: string): number {
  return Number(s.replace(/\D/g, '') || 0)
}

/**
 * Which of a channel's external agents a session is: the lane the harness
 * labels itself with, else the tail of the id the node handed it at
 * `initialize`, since a v7 id's head is a timestamp two agents started
 * together share. Empty for a client that sends neither.
 */
export function externalAgent(clientId: string | null | undefined, lane?: string | null): string {
  if (lane) return lane
  return clientId ? `agent ${clientId.slice(-8)}` : ''
}

/** A usage window's length: "5h", "7d", "90m". */
export function formatWindow(minutes: number): string {
  if (minutes % 1440 === 0) return `${minutes / 1440}d`
  if (minutes % 60 === 0) return `${minutes / 60}h`
  return `${minutes}m`
}

/**
 * Which limit a snapshot is: the provider alone for the plan's own limit,
 * else the provider and the other limit's name, so a fallback snapshot does
 * not pass for the plan's own.
 */
export function codexLimitLabel(l: CodexLimits): string {
  return l.limit_id === 'codex' ? l.provider : `${l.provider} · ${l.limit_name ?? l.limit_id}`
}

/**
 * A subscription's limits as lines a person reads: each window as "42% of
 * 5h used · resets in 2h", the credits when the plan has them, a refusal as
 * "limit reached (rate_limit_reached)", and when the gateway last heard. A
 * window whose reset has passed reads as reset, not as still used. A refusal
 * over a window is no longer shown once every reported window has reset; any
 * other refusal (credits depleted, say) stands until a newer snapshot says
 * otherwise, since a window resetting does not change it.
 */
/** Refusal reasons a window resetting clears; every other reason outlasts it. */
const WINDOW_BOUND_REFUSALS = new Set(['rate_limit_reached'])

export function formatCodexLimits(l: CodexLimits, now = Date.now()): string[] {
  const lines: string[] = []
  const windows = [l.primary, l.secondary].filter((w): w is LimitWindow => w !== null)
  const reset = (w: LimitWindow) => w.resets_at !== null && w.resets_at * 1000 <= now
  const allReset = windows.length > 0 && windows.every(reset)
  const cleared = allReset && l.reached_type !== null && WINDOW_BOUND_REFUSALS.has(l.reached_type)
  if (l.reached_type && !cleared) lines.push(`limit reached (${l.reached_type})`)
  for (const w of windows) {
    const len = w.window_minutes ? formatWindow(w.window_minutes) : null
    if (reset(w)) {
      lines.push(`${len ? `${len} window` : 'window'} reset since (was ${Math.round(w.used_percent)}%)`)
      continue
    }
    const of = len ? ` of ${len}` : ''
    const resets = w.resets_at === null ? '' : ` · resets in ${formatDuration(w.resets_at * 1000 - now)}`
    lines.push(`${Math.round(w.used_percent)}%${of} used${resets}`)
  }
  const c = l.credits
  if (c?.unlimited) lines.push('credits unlimited')
  else if (c?.balance) lines.push(`credits ${c.balance}`)
  else if (c?.has_credits === false) lines.push('no credits')
  lines.push(`observed ${formatAge(l.observed_ms, now)} ago`)
  return lines
}
