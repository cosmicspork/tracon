import { expect, test } from 'bun:test'
import { ApiError } from './api'
import { allAnswered, kept, wantsLogin } from './snapshot'

const ok = <T,>(value: T): PromiseSettledResult<T> => ({ status: 'fulfilled', value })
const bad = (reason: unknown): PromiseSettledResult<never> => ({ status: 'rejected', reason })

test('a value that arrived is taken', () => {
  expect(kept(ok(3), 9)).toBe(3)
})

test('an endpoint that failed keeps the last snapshot rather than discarding it', () => {
  // The regression: one failed fetch used to null out `mesh`, and the Nodes
  // screen then reported "no hub configured" until the stream reconnected.
  const mesh = { hub: { state: 'connected' } }
  expect(kept(bad(new Error('network')), mesh)).toBe(mesh)
  expect(kept<typeof mesh | null>(bad(new Error('network')), null)).toBeNull()
})

test('a 401 from any endpoint is the node asking for a login', () => {
  expect(wantsLogin([ok(1), bad(new ApiError(401, 'log in'))])).toBe(true)
})

test('an unreachable node is not a node asking for a login', () => {
  expect(wantsLogin([ok(1), bad(new Error('network'))])).toBe(false)
  expect(wantsLogin([ok(1), bad(new ApiError(503, 'down'))])).toBe(false)
  expect(wantsLogin([ok(1), ok(2)])).toBe(false)
})

test('a partial load is not a complete one', () => {
  expect(allAnswered([ok(1), ok(2)])).toBe(true)
  expect(allAnswered([ok(1), bad(new Error('network'))])).toBe(false)
})
