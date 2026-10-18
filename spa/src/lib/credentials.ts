import type { CredentialSummary } from './types'

/**
 * Which repositories a credential is lent to, as Settings shows it: what its
 * `repos` says, then the configured entries that reaches now, so a
 * `group/**` prefix is never wider than the page admits. Null when it is lent
 * to none.
 */
export function credentialReach(c: Pick<CredentialSummary, 'repos' | 'covers'>): string | null {
  if (!c.repos?.length) return null
  const covers = c.covers?.length ? `covers ${c.covers.join(', ')}` : 'covers no configured repository'
  return `lent to ${c.repos.join(', ')} · ${covers}`
}
