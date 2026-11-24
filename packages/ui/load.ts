// A list a screen fetched is in one of three states, and each says something
// different: still loading, failed to load, or answered (possibly with
// nothing). Showing the empty state for the first two claims the node said
// "nothing" when it has said nothing yet, or said something went wrong.

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
