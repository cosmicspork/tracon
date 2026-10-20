// Which commit an approval ships, said before anyone approves: the agent's
// own, under its SHA and with its signature, or a new one the node makes,
// which is never signed (#551).
import type { ReviewIntent, ReviewOutputs } from './types'

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

/**
 * The message a squash carries when nobody edits it, as the node's
 * `message_for` derives it: the agent's `message`, else the message of the
 * one commit of its the squash combines. Empty when there is neither: a
 * change's title and description never become a commit message (#592).
 */
export function proposedMessage(intent: ReviewIntent): string {
  return (intent.forge.commit ?? ownMessage(intent)).trim()
}

/** The message the node squashes with for an approval sending `outputs`. */
export function shippedMessage(outputs: ReviewOutputs, intent: ReviewIntent): string {
  return outputs.commit !== undefined ? outputs.commit.trim() : ownMessage(intent)
}

/** The agent's own commit message, for a revision whose squash combines one commit. */
function ownMessage(intent: ReviewIntent): string {
  return intent.own_message ?? intent.as_written ?? ''
}
