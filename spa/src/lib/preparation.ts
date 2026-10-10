import type { PreparationPreview } from './types'

/** One line: what preparation would run, and in what. */
export function preparationLine(p: PreparationPreview): string {
  const steps = [p.install ?? 'no lockfile to install', ...p.prepare]
  return `${steps.join(', then ')} · in ${p.image_source}`
}

/** The first thing that would stop preparation, in a line, or null when nothing would. */
export function preparationBlock(p: PreparationPreview | null): string | null {
  if (!p || p.ready) return null
  const first = p.incompatible.find((i) => i.blocking)
  if (!first) return 'preparation would stop'
  const more = p.incompatible.filter((i) => i.blocking).length - 1
  return `preparation would stop: ${first.source} · ${first.item} ${first.reason}${more > 0 ? ` (+${more} more)` : ''}`
}

/** How the incompatibilities read at a glance. */
export function incompatibilityCount(p: PreparationPreview): string | null {
  const blocking = p.incompatible.filter((i) => i.blocking).length
  const passed = p.incompatible.length - blocking
  const parts: string[] = []
  if (blocking) parts.push(`${blocking} would stop preparation`)
  if (passed) parts.push(`${passed} not honoured`)
  return parts.length ? parts.join(' · ') : null
}
