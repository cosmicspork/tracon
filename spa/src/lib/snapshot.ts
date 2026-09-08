// Merging a batch of snapshot fetches into the store. Split out of
// `store.svelte.ts` so it can be tested without a Svelte runtime.
import { ApiError } from './api'

/**
 * The value an endpoint returned, or `keep` when it failed. One endpoint
 * failing must not discard what the others answered with: a snapshot dropped
 * here is not refetched until the stream reconnects, so a screen can spend the
 * life of the window rendering a value that never arrived as though it were
 * the node's answer.
 */
export function kept<T>(r: PromiseSettledResult<T>, keep: T): T {
  return r.status === 'fulfilled' ? r.value : keep
}

/**
 * Whether any of these failed with the node asking for a login. A node asking
 * for a login is not a node that is down, and the two want different screens.
 */
export function wantsLogin(results: PromiseSettledResult<unknown>[]): boolean {
  return results.some((r) => r.status === 'rejected' && r.reason instanceof ApiError && r.reason.status === 401)
}

/** Whether every endpoint answered. */
export function allAnswered(results: PromiseSettledResult<unknown>[]): boolean {
  return results.every((r) => r.status === 'fulfilled')
}
