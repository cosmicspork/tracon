import { describe, expect, test } from 'bun:test'
import { ReviewDraftConflict } from './api'
import { ReviewDraftSync, type DraftState } from './reviewDraft'
import type { ReviewDraft, ReviewDraftFields } from './types'

/** A node holding one draft, with the same compare-and-set as the real one. */
function node() {
  let held: ReviewDraft | null = null
  const save = async (
    id: string,
    body: { base_version: number; revision_id?: string; draft: ReviewDraftFields },
  ) => {
    if ((held?.version ?? 0) !== body.base_version) throw new ReviewDraftConflict(held)
    held = {
      review_id: id,
      revision_id: body.revision_id ?? null,
      draft: body.draft,
      version: body.base_version + 1,
      updated_ms: 0,
    }
    return { version: held.version, updated_ms: 0 }
  }
  return { save, held: () => held }
}

const settle = () => new Promise((r) => setTimeout(r, 5))

describe('review drafts', () => {
  test('feedback left on one device is found on the next', async () => {
    const n = node()
    const states: DraftState[] = []
    const phone = new ReviewDraftSync('r1', 'v1', (s) => states.push(s), n.save, 0)
    phone.loaded(null, {})
    phone.edit({ reason: 'handle the empty case' })
    await settle()
    expect(states.at(-1)).toBe('saved')

    // Reconnect on the desktop: what it loads is what the phone wrote.
    const desktop = new ReviewDraftSync('r1', 'v1', () => {}, n.save, 0)
    const loaded = n.held()
    expect(loaded?.draft.reason).toBe('handle the empty case')
    desktop.loaded(loaded, loaded!.draft)
    // Loading is not an edit: nothing is written back.
    desktop.edit(loaded!.draft)
    await settle()
    expect(n.held()?.version).toBe(1)
  })

  test('two devices writing at once are told, not overwritten', async () => {
    const n = node()
    const a = new ReviewDraftSync('r1', 'v1', () => {}, n.save, 0)
    const b = new ReviewDraftSync('r1', 'v1', () => {}, n.save, 0)
    a.loaded(null, {})
    b.loaded(null, {})
    a.edit({ reason: 'from a' })
    await settle()
    b.edit({ reason: 'from b' })
    await settle()
    expect(b.state).toBe('conflict')
    expect(b.conflict?.draft.reason).toBe('from a')
    expect(n.held()?.draft.reason).toBe('from a')
    // Further typing does not write while the conflict stands.
    b.edit({ reason: 'from b, more' })
    await settle()
    expect(n.held()?.draft.reason).toBe('from a')

    await b.keepMine({ reason: 'from b, more' })
    expect(b.state).toBe('saved')
    expect(n.held()?.draft.reason).toBe('from b, more')

    a.edit({ reason: 'from a again' })
    await settle()
    expect(a.state).toBe('conflict')
    expect(a.takeTheirs().reason).toBe('from b, more')
    expect(a.state).toBe('saved')
  })

  test('a stopped draft saves nothing more', async () => {
    const n = node()
    const s = new ReviewDraftSync('r1', 'v1', () => {}, n.save, 20)
    s.loaded(null, {})
    s.edit({ reason: 'typed just before approving' })
    s.stop()
    await new Promise((r) => setTimeout(r, 40))
    expect(n.held()).toBeNull()
  })
})
