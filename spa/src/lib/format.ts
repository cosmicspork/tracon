/** Token counts as the interface shows them: 15024 → "15k", 1_240_000 → "1.24M". */
export function formatTokens(n: number): string {
  if (n < 1000) return String(n)
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`
  return `${(n / 1_000_000).toFixed(2).replace(/\.?0+$/, '')}M`
}

/** "0.4M/2M" — the budget column. */
export function formatBudget(used: number, budget: number): string {
  return `${formatTokens(used)}/${formatTokens(budget)}`
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
