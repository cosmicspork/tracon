// What the continuation panel says about each attempt and how they connect.
// Pure, so the wording is tested once rather than read off a screen.

import { publishedChange, requestName } from './verdicts'
import type { Continuation, ContinuationAttempt } from './types'

const ENDED: Record<string, string> = {
  killed_user: 'stopped',
  budget: 'spent its budget',
  harness_exit: 'harness exited',
  item_close: 'item closed',
  phase_done: 'phase done',
  detached: 'detached',
  incompatible: 'harness incompatible',
  node_restart: 'node restarted',
  provider_exhausted: 'provider exhausted',
  error: 'failed',
}

/** How one attempt stands, in a few words. */
export function attemptState(a: ContinuationAttempt): string {
  if (a.end_reason) {
    const why = ENDED[a.end_reason] ?? a.end_reason.replaceAll('_', ' ')
    return a.end_reason === 'error' && a.last_error ? `${why}: ${a.last_error}` : why
  }
  return a.state.replaceAll('_', ' ')
}

/** Which earlier attempt this one carries on, as a short id, or null for a first try. */
export function carries(a: ContinuationAttempt, all: ContinuationAttempt[]): string | null {
  const from = a.continued_from ?? a.parent_session
  if (!from) return null
  return all.some((b) => b.id === from) ? from.slice(0, 8) : null
}

/** The verbs the panel offers, by what the node says is possible. */
export function verbs(c: Continuation): { continue: boolean; changeApproach: boolean; abandon: boolean } {
  const from = c.actions.continue_from !== null
  return {
    // A plain continuation is offered where the work was interrupted rather
    // than where it went wrong; the other way round, the change comes first.
    continue: from && c.next.kind !== 'change_approach',
    changeApproach: from,
    abandon: c.actions.abandon,
  }
}

/** One entry of a work's links bar: a link, or (no `href`) a value to read. */
export interface WorkLink {
  text: string
  href: string | null
  /** A dim word after it: a review's state, a request's state. */
  note?: string
  /** Shown on hover: what the short text stands for. */
  title?: string
  /** The page this bar is on. */
  here?: boolean
  external?: boolean
}

/** A labelled run of links; a group without a label is its links alone. */
export interface WorkLinkGroup {
  label: string | null
  links: WorkLink[]
}

/**
 * Everything linked to a piece of work, in the order its links bar shows it:
 * Task · Plan doc · Sessions · Review · PR · Branch. `here` is the page the
 * bar is on, so it does not link the task to itself.
 */
export function workLinks(c: Continuation, here: { itemId?: string | null; sessionId?: string | null }): WorkLinkGroup[] {
  const groups: WorkLinkGroup[] = []
  if (c.kind === 'item' && here.itemId !== c.id) {
    groups.push({ label: null, links: [{ text: 'Task', href: `/tasks/${c.id}`, title: c.intent.title || undefined }] })
  }
  if (c.decisions.plan) {
    groups.push({ label: null, links: [{ text: 'Plan doc', href: `/docs/${c.channel}/${c.decisions.plan}`, title: c.decisions.plan }] })
  }
  if (c.attempts.length) {
    groups.push({
      label: 'Sessions',
      links: c.attempts.map((a) => ({
        text: `${a.phase} ${a.id.slice(0, 8)}`,
        href: `/sessions/${a.id}`,
        title: attemptState(a),
        here: a.id === here.sessionId,
      })),
    })
  }
  const reviews = [...c.evidence.reviews].sort((a, b) => a.created_ms - b.created_ms)
  if (reviews.length) {
    groups.push({
      label: null,
      links: reviews.map((r) => ({ text: 'Review', href: `/reviews/${r.id}`, note: r.state.replaceAll('_', ' '), title: r.title })),
    })
  }
  const published = reviews.flatMap((r) => {
    const change = publishedChange(r.publish_result)
    if (!change) return []
    return [{ text: requestName(change.url) ?? 'Pull request', href: change.url, note: r.forge_state ?? undefined, title: change.url, external: true }]
  })
  if (published.length) groups.push({ label: null, links: published })
  if (c.workspace?.branch) groups.push({ label: 'Branch', links: [{ text: c.workspace.branch, href: null }] })
  return groups
}
