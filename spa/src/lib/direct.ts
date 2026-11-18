// A review that pushes straight to its base: no change is opened, the base is
// fast-forwarded to the approved revision, and the forge's CI runs only after
// the push. Said plainly always, and as a warning while `warn_direct` is on.

import type { BaseProtection } from './types'

export type { BaseProtection }

/** The parts of a review's target that say it pushes straight to its base. */
export interface DirectTarget {
  base?: string
  direct?: boolean
  warn_direct?: boolean
}

/** The target as a review row carries it (JSON), or null. */
export function parseTarget(target: string | null | undefined): DirectTarget | null {
  try {
    return JSON.parse(target ?? 'null') as DirectTarget | null
  } catch {
    return null
  }
}

/** The change a forge opens, by provider. */
export function changeNoun(provider: string | undefined): string {
  return provider === 'gitlab' ? 'merge request' : 'pull request'
}

/** The warning a direct push carries while `warn_direct` is on; null otherwise. */
export function directWarning(target: DirectTarget | null, provider: string | undefined): string | null {
  if (!target?.direct || !target.warn_direct) return null
  return `Pushes straight to ${target.base}, no ${changeNoun(provider)}; CI runs after the push`
}

/** The short marker the approval list carries while `warn_direct` is on. */
export function directMarker(target: DirectTarget | null): string | null {
  return target?.direct && target.warn_direct ? `pushes straight to ${target.base}` : null
}

/** What the page says of the base's protection: one line, and how loudly. */
export function protectionLine(protection: BaseProtection | null | undefined): { tone: 'crit' | 'dim' | 'wait'; text: string } | null {
  if (!protection) return null
  if (protection.unknown !== undefined) return { tone: 'wait', text: `protection unknown · ${protection.unknown}` }
  const found = protection.found ?? []
  const what = found.length ? `protected: ${found.join('; ')}` : 'not protected'
  if (protection.rejects) return { tone: 'crit', text: `${protection.rejects} (${what})` }
  return { tone: 'dim', text: what }
}
