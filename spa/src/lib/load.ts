// A list a screen fetched is in one of three states, and each says something
// different: still loading, failed to load, or answered (possibly with
// nothing). Showing the empty state for the first two claims the node said
// "nothing" when it has said nothing yet, or said something went wrong.

import { ApiError } from './api'

/** Whether a fetch has answered yet, and why its latest attempt failed. */
export type LoadStatus = { loaded: boolean; error: string | null }

/**
 * `stale` is a refresh that failed after an answer: the last rows stay, under
 * the error, so a flaky connection does not blank a list it already showed.
 */
export type LoadPhase = 'loading' | 'failed' | 'stale' | 'ready'

export function loadPhase(s: LoadStatus): LoadPhase {
  if (s.error !== null) return s.loaded ? 'stale' : 'failed'
  return s.loaded ? 'ready' : 'loading'
}

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

export const LOADING: LoadStatus = { loaded: false, error: null }
export const LOADED: LoadStatus = { loaded: true, error: null }
