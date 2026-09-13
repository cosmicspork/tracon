import { expect, test } from 'bun:test'
import { DRAFT_DEBOUNCE_MS, draftBox, type DraftTimers } from './draft'

/** A clock the test advances by hand, so the debounce is tested rather than waited out. */
function fakeTimers() {
  let next = 1
  const due = new Map<number, { fn: () => void; at: number }>()
  let now = 0
  const timers: DraftTimers = {
    set: (fn, ms) => {
      const id = next++
      due.set(id, { fn, at: now + ms })
      return id
    },
    clear: (handle) => {
      due.delete(handle as number)
    },
  }
  const advance = (ms: number) => {
    now += ms
    for (const [id, t] of [...due]) {
      if (t.at <= now) {
        due.delete(id)
        t.fn()
      }
    }
  }
  return { timers, advance }
}

test('typing is saved once the operator stops, not on every keystroke', () => {
  const { timers, advance } = fakeTimers()
  const saved: string[] = []
  const box = draftBox((t) => void saved.push(t), DRAFT_DEBOUNCE_MS, timers)

  box.typed('h')
  box.typed('ha')
  box.typed('half a thought')
  advance(DRAFT_DEBOUNCE_MS - 1)
  expect(saved).toEqual([])
  advance(1)
  expect(saved).toEqual(['half a thought'])
  expect(box.idle()).toBe(true)
})

test("a late fetch fills an untouched box and never clobbers what is being typed", () => {
  const { timers } = fakeTimers()
  const box = draftBox(() => {}, DRAFT_DEBOUNCE_MS, timers)

  expect(box.restore('half a thought')).toBe('half a thought')
  // Nothing on the node is nothing to restore, whichever way it is spelt.
  expect(box.restore(null)).toBeNull()
  expect(box.restore('')).toBeNull()

  box.typed('mine')
  expect(box.restore('the node had this')).toBeNull()
})

test('sending drops a pending save, so the sent text does not come back', () => {
  const { timers, advance } = fakeTimers()
  const saved: string[] = []
  const box = draftBox((t) => void saved.push(t), DRAFT_DEBOUNCE_MS, timers)

  box.typed('do the thing')
  box.sent()
  advance(DRAFT_DEBOUNCE_MS * 2)
  expect(saved).toEqual([])
  // Sending hands the box back: the next fetch may restore into it again.
  expect(box.restore('something else entirely')).toBe('something else entirely')
})

test('leaving the screen owes the node nothing', () => {
  const { timers, advance } = fakeTimers()
  const saved: string[] = []
  const box = draftBox((t) => void saved.push(t), DRAFT_DEBOUNCE_MS, timers)
  box.typed('half a thought')
  box.dispose()
  advance(DRAFT_DEBOUNCE_MS * 2)
  expect(saved).toEqual([])
})
