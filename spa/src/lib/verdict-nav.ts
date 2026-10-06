// Where a finished verdict leaves the operator.
//
// Deciding a review takes as long as the node takes to publish it, and the
// operator does not have to wait on that screen: the outcome is on the review
// and on the session either way. So the navigation that follows a verdict is
// conditional — it belongs to whoever is still looking at the review, and
// nobody else. Moving someone who has already gone to read something else is
// the bug this prevents. The same holds for any screen a decision is made on.
//
// Kept out of the component because the failure is a race, and a race is not
// visible in markup.

/** The part of the router this needs: where we are, and how to go. */
export interface Nav {
  readonly path: string
  go(path: string): void
}

interface Leaving {
  /** The route the screen was opened from, recorded when its page opened, or
   *  null on a cold load: nothing is behind a page that was typed in. */
  from: string | null
  /** Where to go when there is nowhere to go back to — the review's session,
   *  as every verdict went before. */
  fallback: string
}

/** A decided review, read at `/reviews/{reviewId}`. */
export interface ReviewVerdict extends Leaving {
  reviewId: string
}

/** A decision made on another screen, read at `{prefix}/{id}`. */
export interface ScreenVerdict extends Leaving {
  prefix: string
  id: string
}

export type Verdict = ReviewVerdict | ScreenVerdict

/** The route a decided item is read at. */
export function screenPath(prefix: string, id: string): string {
  return `${prefix}/${id}`
}

/** Whether `path` is still that item's own screen. */
export function onScreen(path: string, prefix: string, id: string): boolean {
  const own = screenPath(prefix, id)
  return path === own || path.startsWith(`${own}/`)
}

/** The route a review is read at. */
export function reviewPath(reviewId: string): string {
  return screenPath('/reviews', reviewId)
}

/** Whether `path` is still that review's own screen. */
export function onReview(path: string, reviewId: string): boolean {
  return onScreen(path, '/reviews', reviewId)
}

function onOwn(path: string, v: Verdict): boolean {
  return 'reviewId' in v ? onReview(path, v.reviewId) : onScreen(path, v.prefix, v.id)
}

/**
 * Where a decision should leave the operator, or null to leave them where
 * they are because the route has moved on while the verdict was in flight.
 */
export function verdictDestination(path: string, v: Verdict): string | null {
  if (!onOwn(path, v)) return null
  // An in-app path and not this same screen: coming back to the screen that
  // was just decided would read as nothing having happened.
  const back =
    v.from !== null &&
    v.from.startsWith('/') &&
    !v.from.startsWith('//') &&
    !onOwn(v.from.split(/[?#]/)[0]!, v)
  return back ? v.from! : v.fallback
}

/**
 * Navigate away from a decided screen, if and only if the operator is still
 * on it. Returns where they were sent, or null if they were left alone.
 */
export function leaveAfterVerdict(nav: Nav, v: Verdict): string | null {
  const to = verdictDestination(nav.path, v)
  if (to !== null) nav.go(to)
  return to
}
