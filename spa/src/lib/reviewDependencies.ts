// What a review publishes after (`submit_review`'s `after`), in the words the
// review page and the home row use. The node reads where each dependency
// stands; this only words it and says what an approval must carry.

import { nameOf } from './names'
import type { ReviewDependencies, ReviewDependency } from './types'

/** The dependencies an approval still waits on. */
export function unmet(deps: ReviewDependencies | null | undefined): ReviewDependency[] {
  return (deps?.after ?? []).filter((d) => !d.satisfied)
}

/** `waits on <name> · <state>` for each one not satisfied, or null when none is. */
export function waitsOnLine(deps: ReviewDependencies | null | undefined): string | null {
  const waiting = unmet(deps)
  if (waiting.length === 0) return null
  return waiting.map((d) => `waits on ${nameOf('review', d.review_id)} · ${d.says}`).join(', ')
}

/**
 * The reason an approval made anyway carries, kept as the verdict's reason
 * on the node; undefined when nothing waits, so an ordinary approval sends
 * none.
 */
export function anywayReason(deps: ReviewDependencies | null | undefined): string | undefined {
  const line = waitsOnLine(deps)
  return line ? `Approved anyway: ${line}` : undefined
}

/** The home row's `after <name>, <name>`, or null for a review that names none. */
export function afterLine(after: string[] | undefined): string | null {
  if (!after?.length) return null
  return `after ${after.map((id) => nameOf('review', id)).join(', ')}`
}
