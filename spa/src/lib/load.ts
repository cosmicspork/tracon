// A list a screen fetched is in one of three states, and each says something
// different: still loading, failed to load, or answered (possibly with
// nothing). Showing the empty state for the first two claims the node said
// "nothing" when it has said nothing yet, or said something went wrong.

import { ApiError } from './api'

import { loadPhase, type LoadStatus } from '@tracon/ui/load'

// The three states, and LoadGate that draws them, are @tracon/ui's.
export { loadPhase, type LoadPhase, type LoadStatus } from '@tracon/ui/load'

/** Whether there is an answer to show, fresh or not. */
export function hasAnswer(s: LoadStatus): boolean {
  const phase = loadPhase(s)
  return phase === 'ready' || phase === 'stale'
}

/** Several fetches a screen needs together: loaded once all are, failed if any is. */
export function combineLoads(...all: LoadStatus[]): LoadStatus {
  return {
    loaded: all.every((s) => s.loaded),
    error: all.find((s) => s.error !== null)?.error ?? null,
  }
}

/** What a settled fetch leaves its status at. A failure keeps `loaded`: the last answer is still the last answer. */
export function settledLoad(previous: LoadStatus, result: PromiseSettledResult<unknown>): LoadStatus {
  return result.status === 'fulfilled'
    ? { loaded: true, error: null }
    : { loaded: previous.loaded, error: errorText(result.reason) }
}

/**
 * Whether a failed read means the thing is not there. Only a 404 says so: a
 * 500 or a dropped connection says nothing about whether it exists, and a
 * screen that calls it "not found" sends the operator looking for the wrong
 * fault.
 */
export function isNotFound(e: unknown): boolean {
  return e instanceof ApiError && e.status === 404
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/**
 * Whether the stream opening should refetch the snapshots. After a drop,
 * frames may have been missed. On the first open it depends on the page's own
 * first fetch: one that failed (a page served from cache while the node was
 * restarting) is not tried again by anything else, so its errors would outlive
 * the outage.
 */
export function refetchOnOpen(wasConnected: boolean, refetching: boolean, loads: LoadStatus[]): boolean {
  if (wasConnected) return true
  return !refetching && loads.some((s) => loadPhase(s) !== 'ready')
}

export const LOADING: LoadStatus = { loaded: false, error: null }
export const LOADED: LoadStatus = { loaded: true, error: null }
