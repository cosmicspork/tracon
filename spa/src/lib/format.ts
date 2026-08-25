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

/** Age since a unix-ms timestamp: "48s", "12m", "2h10". */
export function formatAge(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.floor((now - ms) / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  const rem = m % 60
  return rem > 0 ? `${h}h${String(rem).padStart(2, '0')}` : `${h}h`
}

/** Time remaining until a deadline: "expires 4m", or "expired". */
export function formatExpiry(expiresMs: number, now = Date.now()): string {
  const s = Math.floor((expiresMs - now) / 1000)
  if (s <= 0) return 'expired'
  if (s < 60) return `expires ${s}s`
  return `expires ${Math.ceil(s / 60)}m`
}
