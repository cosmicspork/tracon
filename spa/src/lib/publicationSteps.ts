// A publication attempt, step by step, as the review page shows it after
// Approve. The node names each step (`review::publish::Step`) and says which
// ones this publication will run; the words are the interface's, so they can
// change without a node release.

import type { PublicationProgress, PublicationStep, PublicationView } from './types'

/** A step slower than this shows how long it has taken. */
export const SLOW_STEP_MS = 2000

export type StepState = 'done' | 'running' | 'failed' | 'unknown' | 'pending'

export interface StepRow {
  step: PublicationStep
  label: string
  state: StepState
  /** Seconds the running step has taken, once it is slow. */
  elapsed: string | null
  /** Why it stopped, on the step it stopped at. */
  reason: string | null
  /** The published change, on the last step. */
  url: string | null
}

/** What a forge calls a change. */
function changeNoun(provider: string): string {
  return provider === 'gitlab' ? 'merge request' : 'pull request'
}

/** The change's number from its address (`…/pull/7`, `…/merge_requests/7`), or null. */
export function changeNumber(url: string | null): string | null {
  const last = url?.replace(/\/+$/, '').split('/').pop() ?? ''
  return /^\d+$/.test(last) ? last : null
}

/** What the page says a step is. */
export function stepLabel(step: PublicationStep, p: Pick<PublicationProgress, 'provider' | 'branch' | 'url'>): string {
  switch (step) {
    case 'verifying':
      return 'Checking the reviewed commit'
    case 'preparing_commit':
      return 'Preparing the commit'
    case 'inspecting_forge':
      return 'Looking at the forge'
    case 'pushing':
      return p.branch ? `Pushing ${p.branch}` : 'Pushing'
    case 'confirming_push':
      return 'Confirming the push landed'
    case 'opening_change':
      return `Opening the ${changeNoun(p.provider)}`
    case 'updating_change':
      return 'Updating the description'
    case 'marking_ready':
      return 'Marking it ready'
    case 'commenting':
      return 'Posting the comment'
    case 'published': {
      const number = changeNumber(p.url)
      return number ? `Published: #${number}` : 'Published'
    }
  }
}

/** A loaded publication record, read as the frame it last sent would have said it. */
export function fromView(v: PublicationView, reviewId: string): PublicationProgress | null {
  if (!v.step) return null
  return {
    id: v.id,
    review_id: reviewId,
    node_id: '',
    provider: v.provider,
    branch: v.branch,
    outcome: v.outcome,
    step: v.step,
    step_started_ms: v.step_started_ms ?? null,
    steps: v.steps ?? [],
    reason: ['failed', 'uncertain', 'not_attempted'].includes(v.outcome) ? v.note : null,
    url: v.outcome === 'published' ? v.url : null,
  }
}

/**
 * The newer of what the stream said and what the review was loaded with: a
 * frame missed while the stream was down is made up by a reload, and a
 * reload taken mid-attempt is overtaken by the next frame. At the same step,
 * a settled outcome is newer than a running one.
 */
export function latestProgress(
  live: PublicationProgress | null,
  loaded: PublicationProgress | null,
): PublicationProgress | null {
  if (!live || !loaded) return live ?? loaded
  const at = (p: PublicationProgress) => p.step_started_ms ?? 0
  if (at(live) !== at(loaded)) return at(live) > at(loaded) ? live : loaded
  return loaded.outcome !== 'in_progress' && live.outcome === 'in_progress' ? loaded : live
}

/** Each planned step and where it stands, at `now`. */
export function stepRows(p: PublicationProgress, now: number): StepRow[] {
  const steps: PublicationStep[] = p.steps.length ? [...p.steps] : p.step ? [p.step] : []
  // A step the plan does not name (an older plan) is still shown, where it ran.
  if (p.step && !steps.includes(p.step)) steps.splice(Math.max(steps.length - 1, 0), 0, p.step)
  const published = p.outcome === 'published'
  const current = published ? steps.length : p.step ? steps.indexOf(p.step) : 0
  const stopped: StepState | null =
    p.outcome === 'uncertain' ? 'unknown' : p.outcome === 'failed' || p.outcome === 'not_attempted' ? 'failed' : null
  return steps.map((step, i) => {
    const state: StepState =
      published || i < current ? 'done' : i > current ? 'pending' : (stopped ?? 'running')
    const taken = state === 'running' && p.step_started_ms ? now - p.step_started_ms : 0
    return {
      step,
      label: stepLabel(step, p),
      state,
      elapsed: taken >= SLOW_STEP_MS ? `${Math.floor(taken / 1000)}s` : null,
      reason: state === 'failed' || state === 'unknown' ? p.reason : null,
      url: step === 'published' && published ? p.url : null,
    }
  })
}
