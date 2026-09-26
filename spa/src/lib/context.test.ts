import { expect, test } from 'bun:test'
import { attemptLine, deliveryLine, picksInOrder, withPick, without } from './context'
import type { ContextReceipt, ContextSelection } from './types'

function receipt(over: Partial<ContextReceipt>): ContextReceipt {
  return {
    session_id: 's',
    work_item_id: 'w',
    channel: 'personal',
    revision: 1,
    digest: 'd',
    received: [],
    changes: [],
    created_ms: 0,
    ...over,
  }
}

test('an attempt says whether its context moved and what did not arrive whole', () => {
  const full = { role: 'research', slug: 'ref-a', chars: 10, delivered_chars: 10, delivery: 'full' } as const
  const absent = { role: 'constraints', slug: 'guide-b', chars: 0, delivered_chars: 0, delivery: 'omitted', reason: 'absent' } as const
  expect(attemptLine(receipt({ received: [full] }))).toBe('revision 1 · first attempt')
  expect(attemptLine(receipt({ received: [full], previous_revision: 1 }))).toBe('revision 1 · unchanged')
  expect(
    attemptLine(
      receipt({
        revision: 2,
        previous_revision: 1,
        received: [full, absent],
        changes: [{ kind: 'added', slug: 'guide-b', role: 'constraints', says: '`guide-b` added as constraints' }],
      }),
    ),
  ).toBe('revision 2 · 1 change since revision 1 · 1 not delivered whole')
  expect(attemptLine(receipt({ revision: 3, previous_revision: 2, changes: [] }))).toContain('nothing selected')
  expect(deliveryLine(full)).toBe('delivered in full')
  expect(deliveryLine(absent)).toBe('left out · not on this node')
  expect(deliveryLine({ ...full, delivery: 'partial', reason: 'cap' })).toContain('cut short')
})

test('picks read in role order, and picking a selected document again moves it', () => {
  const selection = {
    picks: [
      { role: 'documents', slug: 'ref-z', note: '' },
      { role: 'brief', slug: 'brief-a', note: '' },
    ],
  } as unknown as ContextSelection
  expect(picksInOrder(selection).map((p) => p.slug)).toEqual(['brief-a', 'ref-z'])
  expect(picksInOrder(null)).toEqual([])

  const moved = withPick(selection.picks, { role: 'constraints', slug: ' ref-z ', note: ' limits ' })
  expect(moved).toEqual([
    { role: 'brief', slug: 'brief-a', note: '' },
    { role: 'constraints', slug: 'ref-z', note: 'limits' },
  ])
  expect(withPick(moved, { role: 'research', slug: '  ', note: '' })).toBe(moved)
  expect(without(moved, 'brief-a').map((p) => p.slug)).toEqual(['ref-z'])
})
