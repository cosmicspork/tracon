// How current the node's copy of a sandboxed session's work is. The harness
// works in its sandbox, at /work; the node takes that tree back at each turn
// end, every minute while a turn runs, and as the sandbox ends, and keeps the
// last copy that came back. `worktree_path` on a session is none of these: it
// is the node's last export (for a review, a diff or a publication), which can
// be hours old, so it is never shown as where the work is.

import { formatDuration } from './format'
import type { FilesSync, Session } from './types'

/** Where a sandboxed session's harness works, which its tool paths start with. */
export const SANDBOX_WORKDIR = '/work'

/**
 * How old the node's copy may get while a turn runs before the page warns:
 * three missed periodic syncs and one sync's own bound (3 × 60 s + 120 s).
 */
export const STALE_AFTER_MS = 3 * 60_000 + 120_000

/**
 * The root the session log strips from tool paths: only one the paths
 * really start with. An external harness works in its host's worktree; any
 * other works at /work in its sandbox.
 */
export function logRoot(session: Pick<Session, 'harness_id' | 'worktree_path'>): string | null {
  return session.harness_id === 'external' ? session.worktree_path : SANDBOX_WORKDIR
}

export interface FilesSyncNote {
  /** For the session header: "node copy 40s old". */
  text: string
  title: string
  /** The node's copy may be missing work: shown as a warning. */
  warn: boolean
}

/**
 * The header's note on the node's copy, or null for a session nothing is
 * taken back from (an external one, or one whose files are mounted).
 * `turnActive`: the copy only falls behind while a turn runs, so between
 * turns an old copy that came back cleanly is not a warning.
 */
export function filesSyncNote(sync: FilesSync | null | undefined, now: number, turnActive: boolean): FilesSyncNote | null {
  if (!sync) return null
  const age = sync.last_ok_ms != null ? formatDuration(Math.max(0, now - sync.last_ok_ms)) : null
  const copy = age ? `node copy ${age} old` : 'no node copy taken back yet'
  const failed =
    sync.failures > 0
      ? `last ${sync.failures === 1 ? 'sync' : `${sync.failures} syncs`} failed${sync.last_error ? `: ${sync.last_error}` : ''}`
      : ''
  if (!sync.live) {
    if (sync.failures > 0)
      return {
        text: 'the sandbox ended with changes the node may not have',
        title: [copy, failed].join(', ') + '. The node keeps the last copy that came back.',
        warn: true,
      }
    return {
      text: age ? `node copy ${age} old` : 'node copy',
      title: 'The sandbox has ended; its work came back to the node as it ended.',
      warn: false,
    }
  }
  const stale = turnActive && sync.last_ok_ms != null && now - sync.last_ok_ms > STALE_AFTER_MS
  const syncing = sync.in_flight ? 'syncing' : ''
  return {
    text: [copy, failed, syncing].filter(Boolean).join(', '),
    title: stale
      ? `The node has not taken the sandbox's work back for ${age}. If the sandbox is lost now, what changed since is lost with it.`
      : "The node's copy of the work in the sandbox: taken back at each turn end, every minute while a turn runs, and as the sandbox ends.",
    warn: stale || sync.failures > 0,
  }
}
