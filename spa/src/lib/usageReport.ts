// The Usage page's breakdown: the query it sends, the rows it shows, the
// file it offers. Pure, so it tests flat.

import { localDate, type UsageView, usageWindow } from './routes'
import type { UsageTotal } from './types'

/**
 * The `/api/usage` query for `view`; `/api/usage.csv` takes the same one.
 * It carries the browser's offset at the window's start, so the node keys
 * days and names the file in the zone the dates were picked in. One offset
 * holds for the whole window: a DST change inside it can put one day's
 * boundary an hour off.
 */
export function usageParams(view: UsageView, now = Date.now()): URLSearchParams {
  const { since, until } = usageWindow(view, now)
  const q = new URLSearchParams({
    since_ms: String(since),
    tz_offset_minutes: String(-new Date(since).getTimezoneOffset()),
  })
  if (until !== null) q.set('until_ms', String(until))
  if (view.provider) q.set('provider', view.provider)
  if (view.group !== 'model') q.set('group', view.group)
  return q
}

/** The address of the current view's CSV. */
export function usageCsvHref(view: UsageView, now = Date.now()): string {
  return `/api/usage.csv?${usageParams(view, now)}`
}

/** What the breakdown's third column says for a row: its model, day or session. */
export function usageKey(row: UsageTotal, view: UsageView): string {
  if (view.group === 'day') return row.day ?? '—'
  if (view.group === 'session') {
    if (!row.session_id) return 'outside a session'
    return row.title ? `${row.title}${row.review ? ` · ${row.review}` : ''}` : row.session_id
  }
  return row.model ?? 'unknown model'
}

/** The breakdown's heading, after what keys its rows. */
export function usageHeading(view: UsageView): string {
  return `By provider and ${view.group}`
}

/** The span a view covers, as a person reads it: "2026-10-01 – 2026-10-15". */
export function usageSpan(view: UsageView, now = Date.now()): string {
  const { since, until } = usageWindow(view, now)
  return `${localDate(since)} – ${until === null ? 'now' : localDate(until - 1)}`
}

/**
 * The providers the filter offers: those this node knows, those the rows
 * name (a provider since removed still has history), and the chosen one, so
 * a filter that matched nothing can still be cleared.
 */
export function usageProviders(known: string[], rows: UsageTotal[], chosen: string | null): string[] {
  const all = new Set([...known, ...rows.map((r) => r.provider)])
  if (chosen) all.add(chosen)
  return [...all].sort()
}

/** `$1.23`, or `unpriced` when the provider carries no price. */
export function usageCost(cost: number | null): string {
  return cost === null ? 'unpriced' : `$${cost.toFixed(2)}`
}
