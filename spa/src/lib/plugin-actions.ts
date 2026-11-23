// Settings › Plugins: what a row says after "Check for update" or a
// rollback, read from the node's answer. Installing and removing stay with
// `tracon plugin`.

import { ApiError } from './api'
import type { PluginBuild, PluginOffered, PluginSummary } from './types'

/** What a plugin's row says after an update check or a rollback. */
export type PluginNote =
  | { kind: 'current'; text: string }
  | { kind: 'card'; text: string; approvalId: string }
  | { kind: 'rolled'; text: string }
  /** The node refused, and asking it again would be refused the same way. */
  | { kind: 'refused'; text: string }
  | { kind: 'error'; text: string }

/** The update card waiting on the operator for `plugin`, if one is. */
export function waitingCard(plugin: PluginSummary): string | null {
  const pending = plugin.pending
  return pending && pending.approval_state === 'pending' ? pending.approval_id : null
}

/** What the row says of the node's answer to `POST /api/plugins/{name}/update`. */
export function updateNote(offered: PluginOffered): PluginNote {
  if (offered.up_to_date) return { kind: 'current', text: `Up to date: ${offered.version} is the latest release.` }
  const from = offered.from_version ? ` from ${offered.from_version}` : ''
  return {
    kind: 'card',
    approvalId: offered.approval_id,
    text: `An update card for ${offered.version}${from} is waiting on ${offered.channel}. Nothing is installed until it is allowed.`,
  }
}

/** What the row says when the update check failed: the node's own message.
    A plugin installed from a local archive is refused for good, so the row
    offers no retry. */
export function updateFailure(plugin: PluginSummary, cause: unknown): PluginNote {
  const text = cause instanceof Error ? cause.message : String(cause)
  if (cause instanceof ApiError && cause.status === 422 && plugin.repo.startsWith('local:')) return { kind: 'refused', text }
  return { kind: 'error', text }
}

/** The row after a rollback, from the node's answer: its builds swapped. */
export function rolledBack(plugin: PluginSummary, now: { active: PluginBuild; previous: PluginBuild }): PluginSummary {
  return {
    ...plugin,
    active: plugin.active ? { ...plugin.active, version: now.active.version } : null,
    previous: now.previous,
  }
}

export function rollbackNote(now: { active: PluginBuild; previous: PluginBuild }): PluginNote {
  return {
    kind: 'rolled',
    text: `Rolled back to ${now.active.version}; ${now.previous.version} is now the previous version. Its processes stopped, and the next use starts ${now.active.version}.`,
  }
}
