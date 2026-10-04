// Where a finished verdict leaves the operator.
//
// Deciding a review takes as long as the node takes to publish it, and the
// operator does not have to wait on that screen: the outcome is on the review
// and on the session either way. So the navigation that follows a verdict is
// conditional — it belongs to whoever is still looking at the review, and
// nobody else. Moving someone who has already gone to read something else is
// the bug this prevents.
//
// Kept out of the component because the failure is a race, and a race is not
// visible in markup.

/** The part of the router this needs: where we are, and how to go. */
export interface Nav {
  readonly path: string
  go(path: string): void
}

export interface Verdict {
  /** The review the decision was started from. */
  reviewId: string
  /** The route the review was opened from, recorded when its page opened, or
   *  null on a cold load: nothing is behind a page that was typed in. */
  from: string | null
  /** Where to go when there is nowhere to go back to — the review's session,
   *  as every verdict went before. */
  fallback: string
}

/** The route a review is read at. */
export function reviewPath(reviewId: string): string {
  return `/reviews/${reviewId}`
}

/** Whether `path` is still that review's own screen. */
export function onReview(path: string, reviewId: string): boolean {
  const own = reviewPath(reviewId)
  return path === own || path.startsWith(`${own}/`)
}

/**
 * Where a decided review should leave the operator, or null to leave them
 * where they are because the route has moved on while the verdict was in
 * flight.
 */
export function verdictDestination(path: string, v: Verdict): string | null {
  if (!onReview(path, v.reviewId)) return null
  // An in-app path and not this same review: coming back to the screen that
  // was just decided would read as nothing having happened.
  const back =
    v.from !== null &&
    v.from.startsWith('/') &&
    !v.from.startsWith('//') &&
    !onReview(v.from.split(/[?#]/)[0]!, v.reviewId)
  return back ? v.from! : v.fallback
}

/**
 * Navigate away from a decided review, if and only if the operator is still
 * on it. Returns where they were sent, or null if they were left alone.
 */
export function leaveAfterVerdict(nav: Nav, v: Verdict): string | null {
  const to = verdictDestination(nav.path, v)
  if (to !== null) nav.go(to)
  return to
}
