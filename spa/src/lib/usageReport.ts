// The Usage page's breakdown: the query it sends, the rows it shows, the
// file it offers. Pure, so it tests flat.

import { nameOf } from './names'
import { localDate, type UsageView, usageWindow } from './routes'
import type { InfraSpend, InfraTotal, UsageTotal } from './types'

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
    return row.title ? `${row.title}${row.review ? ` · ${row.review}` : ''}` : nameOf('session', row.session_id)
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

/** How the Infrastructure section groups its rows. */
export type InfraGroup = 'day' | 'channel' | 'session' | 'size'

/** The `/api/usage/infra` query: the breakdown's window and zone, grouped by `group`. */
export function infraParams(view: UsageView, group: InfraGroup, now = Date.now()): URLSearchParams {
  const { since, until } = usageWindow(view, now)
  const q = new URLSearchParams({
    since_ms: String(since),
    tz_offset_minutes: String(-new Date(since).getTimezoneOffset()),
    group,
  })
  if (until !== null) q.set('until_ms', String(until))
  return q
}

/** The address of the Infrastructure section's CSV. */
export function infraCsvHref(view: UsageView, group: InfraGroup, now = Date.now()): string {
  return `/api/usage/infra.csv?${infraParams(view, group, now)}`
}

/** What a row of the Infrastructure section is keyed by. */
export function infraKey(row: InfraTotal, group: InfraGroup): string {
  if (group === 'day') return row.day ?? '—'
  if (group === 'channel') return row.channel || 'no channel'
  if (group === 'size') return row.size || 'default size'
  if (row.session_id) return nameOf('session', row.session_id)
  return row.kind ? `${row.kind} run` : 'outside a session'
}

/** Sandbox hours as a person reads them: `45m`, `3.2h`. */
export function formatHours(hours: number): string {
  return hours < 1 ? `${Math.round(hours * 60)}m` : `${hours.toFixed(1)}h`
}

/** Today's spend, against the limit when one is set: `$1.20 of $5.00 today`. */
export function infraSpendLine(spend: InfraSpend): string {
  const today = `$${spend.today_usd.toFixed(2)}`
  return spend.limit_usd === null ? `${today} today, no limit` : `${today} of $${spend.limit_usd.toFixed(2)} today`
}
