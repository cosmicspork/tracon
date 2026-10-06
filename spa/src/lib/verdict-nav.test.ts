import { expect, test } from 'bun:test'
import {
  leaveAfterVerdict,
  onReview,
  onScreen,
  screenPath,
  verdictDestination,
  type Nav,
} from './verdict-nav'

/** The router as this sees it: a path that only its own `go` changes. */
function fakeNav(path: string) {
  const nav: Nav & { path: string; went: string[] } = {
    path,
    went: [],
    go(to: string) {
      nav.went.push(to)
      nav.path = to.split(/[?#]/)[0]!
    },
  }
  return nav
}

/** A decision whose API call the test resolves by hand. */
function delayedDecision(nav: Nav, v: Parameters<typeof leaveAfterVerdict>[1]) {
  let publish: () => void = () => {}
  const sent = new Promise<void>((resolve) => (publish = resolve))
  const done = sent.then(() => leaveAfterVerdict(nav, v))
  return { publish: () => publish(), done }
}

test('a verdict that lands after the operator has gone elsewhere does not move them', async () => {
  const nav = fakeNav('/reviews/r1')
  const decision = delayedDecision(nav, {
    reviewId: 'r1',
    from: '/work/w1',
    fallback: '/sessions/s1',
  })

  // The verdict and the publish are in flight; the operator goes to read
  // something else rather than waiting on the review screen.
  nav.go('/sessions/s9')
  decision.publish()
  expect(await decision.done).toBeNull()

  expect(nav.path).toBe('/sessions/s9')
  expect(nav.went).toEqual(['/sessions/s9'])
})

test('a verdict that lands on the review goes back to where it was opened from', async () => {
  const nav = fakeNav('/reviews/r1')
  const decision = delayedDecision(nav, {
    reviewId: 'r1',
    from: '/work/w1',
    fallback: '/sessions/s1',
  })

  decision.publish()
  expect(await decision.done).toBe('/work/w1')
  expect(nav.path).toBe('/work/w1')
})

test('with nowhere to go back to, a verdict goes where it always went', () => {
  const v = { reviewId: 'r1', from: null, fallback: '/sessions/s1' }
  expect(verdictDestination('/reviews/r1', v)).toBe('/sessions/s1')
  // Home is the fallback for a verdict the node did not publish.
  expect(verdictDestination('/reviews/r1', { ...v, fallback: '/' })).toBe('/')
})

test('the review is never the way back to itself', () => {
  const v = { reviewId: 'r1', fallback: '/sessions/s1' }
  // Reloaded on the review, or arrived from one of its own sub-routes.
  expect(verdictDestination('/reviews/r1', { ...v, from: '/reviews/r1' })).toBe('/sessions/s1')
  expect(verdictDestination('/reviews/r1', { ...v, from: '/reviews/r1?tab=diff' })).toBe(
    '/sessions/s1',
  )
  // Another review is somewhere else, and is a place to go back to.
  expect(verdictDestination('/reviews/r1', { ...v, from: '/reviews/r2' })).toBe('/reviews/r2')
})

test('a recorded origin that is not an in-app path is not navigated to', () => {
  const v = { reviewId: 'r1', fallback: '/sessions/s1' }
  expect(verdictDestination('/reviews/r1', { ...v, from: '//evil.example' })).toBe('/sessions/s1')
  expect(verdictDestination('/reviews/r1', { ...v, from: 'https://evil.example' })).toBe(
    '/sessions/s1',
  )
  expect(verdictDestination('/reviews/r1', { ...v, from: '' })).toBe('/sessions/s1')
})

test('the review screen is its own path and whatever hangs off it', () => {
  expect(onReview('/reviews/r1', 'r1')).toBe(true)
  expect(onReview('/reviews/r1/files/a.ts', 'r1')).toBe(true)
  expect(onReview('/reviews/r10', 'r1')).toBe(false)
  expect(onReview('/sessions/r1', 'r1')).toBe(false)
  expect(onReview('/', 'r1')).toBe(false)
})

test('any screen under a prefix leaves the same way a review does', async () => {
  const v = { prefix: '/approvals', id: 'a1', from: '/sessions/s1', fallback: '/' }
  expect(verdictDestination('/approvals/a1', v)).toBe('/sessions/s1')
  expect(verdictDestination('/approvals/a1', { ...v, from: '/approvals/a1?x=1' })).toBe('/')
  // A review with the same id is a different screen.
  expect(verdictDestination('/reviews/a1', v)).toBeNull()
  expect(verdictDestination('/approvals/a10', v)).toBeNull()

  const nav = fakeNav('/approvals/a1')
  const decision = delayedDecision(nav, v)
  nav.go('/work')
  decision.publish()
  expect(await decision.done).toBeNull()
  expect(nav.path).toBe('/work')
})

test('a screen is its own path under its prefix and whatever hangs off it', () => {
  expect(screenPath('/approvals', 'a1')).toBe('/approvals/a1')
  expect(onScreen('/approvals/a1/raw', '/approvals', 'a1')).toBe(true)
  expect(onScreen('/approvals/a10', '/approvals', 'a1')).toBe(false)
  expect(onScreen('/reviews/a1', '/approvals', 'a1')).toBe(false)
})
