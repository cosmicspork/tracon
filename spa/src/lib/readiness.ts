import type { PathReadiness, RepoReadiness } from './types'

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
