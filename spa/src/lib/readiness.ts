import type { PathReadiness, ReadinessGap, RepoReadiness } from './types'

export const PATHS = ['investigate', 'verify', 'publish'] as const

/** The paths in order, each with the one line that says where it stands. */
export function summarize(r: RepoReadiness): { purpose: PathReadiness['purpose']; ready: boolean; line: string }[] {
  return PATHS.map((purpose) => {
    const p = r[purpose]
    const first = p.missing[0]
    const more = p.missing.length > 1 ? ` (+${p.missing.length - 1} more)` : ''
    return {
      purpose,
      ready: p.ready,
      line: p.ready ? 'ready' : `${first?.message ?? 'not ready'}${more}`,
    }
  })
}

/** What a path lacks that the one before it does not: the reason it is a
    separate step, said once rather than repeated down the list. */
export function ownGaps(r: RepoReadiness, purpose: PathReadiness['purpose']): PathReadiness['missing'] {
  const index = PATHS.indexOf(purpose)
  if (index <= 0) return r[purpose].missing
  const before = new Set(r[PATHS[index - 1]].missing.map((gap) => gap.key))
  return r[purpose].missing.filter((gap) => !before.has(gap.key))
}

/** The paths a repository cannot take yet, in order. */
export function unready(r: RepoReadiness): PathReadiness['purpose'][] {
  return PATHS.filter((purpose) => !r[purpose].ready)
}

/** The banner's first words: what cannot happen here. Empty when nothing is
    missing, so a ready repository says nothing at all. */
export function headline(r: RepoReadiness): string {
  const paths = unready(r)
  if (!paths.length) return ''
  // Investigating is the floor: when it fails, nothing can start.
  if (paths[0] === 'investigate') return 'No session can start here'
  return `Not ready to ${paths.join(' or ')}`
}

/** Every gap once, under the first path it stops. */
export function allGaps(r: RepoReadiness): { purpose: PathReadiness['purpose']; gap: ReadinessGap }[] {
  return PATHS.flatMap((purpose) => ownGaps(r, purpose).map((gap) => ({ purpose, gap })))
}

/** Where a gap is closed, for the gaps a page can send someone to. A missing
    directory or `origin` remote is fixed in the repository itself, so those
    have none. */
export function remedy(key: string): { href: string; label: string } | null {
  switch (key) {
    case 'launch':
      return { href: '/settings#system', label: 'Review setup' }
    case 'checks':
      return { href: '/settings#channels', label: 'Add checks in Settings' }
    case 'image':
      return { href: '/settings#channels', label: 'Build it in Settings' }
    case 'credential':
      return { href: '/settings#connections', label: 'Bind a token in Settings' }
    default:
      return null
  }
}

/** Why no session can work here at all — investigating is the least a
    session needs — or null when it can, or the node has not said. */
export function cannotWork(r: RepoReadiness | null): string | null {
  if (!r || r.investigate.ready) return null
  return summarize(r)[0].line
}
