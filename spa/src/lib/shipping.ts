// Which commit an approval ships, said before anyone approves: the agent's
// own, under its SHA and with its signature, or a new one the node makes,
// which is never signed (#551).
import type { ReviewIntent } from './types'

export interface Shipping {
  /** The agent's head ships as written. */
  own: boolean
  /** The agent's signed commits a new commit replaces, oldest first: their signatures are dropped. */
  dropped: string[]
}

/**
 * What ships for `intent` when the squash carries `message`, or, under
 * `keep`, with the commit messages in `edited` (by sha) changed. A squash
 * ships the agent's head only with the message it already has; an edited
 * commit is remade, and so is every commit after it.
 */
export function shipping(intent: ReviewIntent, message: string, edited: Record<string, string>): Shipping {
  const signed = new Set(intent.signed ?? [])
  if (intent.squash_onto !== undefined) {
    const own = intent.as_written !== undefined && message.trim() === intent.as_written
    return { own, dropped: own ? [] : (intent.commits ?? []).map((c) => c.sha).filter((sha) => signed.has(sha)) }
  }
  const commits = intent.commits ?? []
  const first = commits.findIndex((c) => edited[c.sha] !== undefined)
  if (first < 0) return { own: true, dropped: [] }
  return { own: false, dropped: commits.slice(first).map((c) => c.sha).filter((sha) => signed.has(sha)) }
}
