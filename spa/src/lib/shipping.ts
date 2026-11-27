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

/** "1 commit", "2 commits". */
function commitCount(n: number): string {
  return n === 1 ? '1 commit' : `${n} commits`
}

/** "aaaa1111", "aaaa1111 and bbbb2222", "a, b and c": shas as the page shortens them. */
export function shaList(shas: string[]): string {
  const short = shas.map((sha) => sha.slice(0, 8))
  if (short.length <= 1) return short.join('')
  return `${short.slice(0, -1).join(', ')} and ${short[short.length - 1]}`
}

/**
 * The one plain line On the forge says about what an approval pushes, for
 * `intent` shipping as `ships` with the commit messages in `edited` changed
 * (#1194). It counts every commit that goes, so two kept commits never read
 * as one. The signatures a new commit drops are listed apart.
 */
export function shipLine(intent: ReviewIntent, ships: Shipping, edited: Record<string, string>): string {
  const commits = intent.commits ?? []
  if (intent.squash_onto !== undefined) {
    const head = commits[commits.length - 1]
    if (ships.own && head) {
      const signed = (intent.signed ?? []).includes(head.sha)
      return `pushes one commit of the reviewed tree · the agent's own ${head.sha.slice(0, 8)}${signed ? ', signature kept' : ''}`
    }
    return 'pushes one commit of the reviewed tree'
  }
  const reworded = commits.filter((c) => edited[c.sha] !== undefined).length
  if (reworded > 0) {
    const messages = reworded === 1 ? '1 message' : `${reworded} messages`
    return `pushes ${commitCount(commits.length)} · ${messages} edited here${ships.dropped.length ? ' · reworded commits lose their signatures' : ''}`
  }
  const signed = commits.map((c) => c.sha).filter((sha) => (intent.signed ?? []).includes(sha))
  const kept =
    signed.length === 0
      ? ''
      : signed.length === commits.length
        ? commits.length === 1
          ? ' · signature kept'
          : ' · signatures kept'
        : ` · ${signed.length === 1 ? 'signature' : 'signatures'} on ${shaList(signed)} kept`
  return `pushes ${commitCount(commits.length)} as written${kept}`
}

/** What the signatures a new commit replaces come to, or '' when none are dropped. */
export function droppedNote(ships: Shipping): string {
  if (ships.own || ships.dropped.length === 0) return ''
  const many = ships.dropped.length > 1
  return `${many ? 'The signatures on' : 'The signature on'} ${shaList(ships.dropped)} ${many ? 'are' : 'is'} dropped: a commit the node makes is never signed.`
}
