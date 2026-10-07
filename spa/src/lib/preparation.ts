import type { PreparationPreview } from './types'

/** One line: what preparation would run, and in what. */
export function preparationLine(p: PreparationPreview): string {
  const steps = [p.install ?? 'no lockfile to install', ...p.prepare]
  return `${steps.join(', then ')} · in ${p.image_source}`
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
