import { describe, expect, test } from 'bun:test'
import { ApiError } from './api'
import { rollbackNote, rolledBack, updateFailure, updateNote, waitingCard } from './plugin-actions'
import type { PluginSummary } from './types'

const plugin = (over: Partial<PluginSummary> = {}): PluginSummary => ({
  name: 'sql-review',
  repo: 'example-org/sql-review',
  state: 'installed',
  active: { version: '0.3.1', tools: [] },
  previous: { version: '0.3.0', digest: 'aa' },
  pending: null,
  instances: [],
  ...over,
})

describe('plugin actions', () => {
  test('an active build that is the latest release is up to date', () => {
    expect(updateNote({ up_to_date: true, version: '0.3.1', digest: 'bb' })).toEqual({
      kind: 'current',
      text: 'Up to date: 0.3.1 is the latest release.',
    })
  })

  test('a raised card names its version and links to it', () => {
    const note = updateNote({
      name: 'sql-review',
      version: '0.4.0',
      from_version: '0.3.1',
      digest: 'cc',
      approval_id: 'ap-1',
      channel: 'personal',
      new: true,
    })
    expect(note.kind).toBe('card')
    expect(note.kind === 'card' && note.approvalId).toBe('ap-1')
    expect(note.text).toContain('0.4.0 from 0.3.1 is waiting on personal')
  })

  test("a local archive's 422 is the node's message, with no retry; other failures may be retried", () => {
    const local = plugin({ repo: 'local:sql-review-0.1.0-x86_64-unknown-linux-musl.tar.gz', previous: null })
    const refusal = new ApiError(422, 'sql-review was installed from a local archive; add the new archive again to update it')
    expect(updateFailure(local, refusal)).toEqual({ kind: 'refused', text: refusal.message })
    // A release from another plugin is a 422 too, but not one for good.
    const other = new ApiError(422, "example-org/sql-review's latest release is plugin hoja, not sql-review")
    expect(updateFailure(plugin(), other)).toEqual({ kind: 'error', text: other.message })
    expect(updateFailure(plugin(), new ApiError(404, 'no plugin sql-review is installed')).kind).toBe('error')
    expect(updateFailure(plugin(), new TypeError('Failed to fetch'))).toEqual({ kind: 'error', text: 'Failed to fetch' })
  })

  test('only a card still pending blocks the buttons', () => {
    expect(waitingCard(plugin())).toBeNull()
    const pending = { version: '0.4.0', digest: 'cc', approval_id: 'ap-1' }
    expect(waitingCard(plugin({ pending: { ...pending, approval_state: 'pending' } }))).toBe('ap-1')
    expect(waitingCard(plugin({ pending: { ...pending, approval_state: 'expired' } }))).toBeNull()
  })

  test('a rollback shows the versions the node answered with', () => {
    const now = { active: { version: '0.3.0', digest: 'aa' }, previous: { version: '0.3.1', digest: 'bb' } }
    const row = rolledBack(plugin(), now)
    expect(row.active?.version).toBe('0.3.0')
    expect(row.previous?.version).toBe('0.3.1')
    expect(rollbackNote(now).text).toContain('Rolled back to 0.3.0; 0.3.1 is now the previous version')
  })
})
