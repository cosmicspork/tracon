// What a session's page offers, from the state the node recorded. Each
// action is offered only where the node would take it: Pause needs a running
// supervisor, Resume a paused one, and a suspended session has neither — its
// container is stopped, and the way on is Continue, in its banner.

import { isTerminal, type Askable, type Session } from './types'

export interface SessionControls {
  pause: boolean
  resume: boolean
  stop: boolean
}

const NONE: SessionControls = { pause: false, resume: false, stop: false }

/** The header's Pause, Resume and Stop. `held`: the exhaustion banner carries Resume. */
export function sessionControls(s: Pick<Session, 'state'>, held = false): SessionControls {
  if (isTerminal(s.state) || s.state === 'suspended') return NONE
  if (s.state === 'paused') return { pause: false, resume: !held, stop: true }
  if (s.state === 'starting') return { pause: false, resume: false, stop: true }
  return { pause: true, resume: false, stop: true }
}

/**
 * The prompt box: whether it takes typing, what its placeholder says, and
 * whether Send queues. `reason` is why a prompt cannot start a turn now;
 * `busy` is a turn or the checks running, when the node holds a prompt for
 * the agent instead of refusing it, so the box stays open and says so.
 */
export function composerState(
  reason: string | null,
  busy: boolean,
): { disabled: boolean; placeholder: string; queues: boolean } {
  if (reason === null) return { disabled: false, placeholder: 'Send a prompt. Drafts are held on the node.', queues: false }
  if (busy) {
    const said = reason.charAt(0).toUpperCase() + reason.slice(1)
    return {
      disabled: false,
      placeholder: `${said}. Queue a message: the agent gets it in its next tracon tool result, or when the turn ends.`,
      queues: true,
    }
  }
  return { disabled: true, placeholder: `Input disabled: ${reason}`, queues: false }
}

/**
 * What a session that ended or was suspended offers in place of a live
 * prompt box: a box that asks it again in its own conversation, or why its
 * conversation cannot be restored (and Continue, a new session, instead).
 * `live` is a session whose own box takes prompts; `none` is one the page has
 * other words for (an external agent, a legacy session) or has not heard
 * about yet.
 */
export type AskMode = { kind: 'live' } | { kind: 'ask' } | { kind: 'unrestorable'; reason: string } | { kind: 'none' }

export function askMode(s: Pick<Session, 'state' | 'harness_id' | 'legacy_ms'>, askable: Askable | null): AskMode {
  if (!isTerminal(s.state) && s.state !== 'suspended') return { kind: 'live' }
  if (s.harness_id === 'external' || s.legacy_ms || !askable) return { kind: 'none' }
  if (askable.restorable) return { kind: 'ask' }
  return { kind: 'unrestorable', reason: askable.reason ?? 'the node did not say why' }
}

/** Whether the header offers Compact: the harness compacts on request, and
 *  the session is idle and live, or can be asked again. */
export function compactOffered(s: Pick<Session, 'state' | 'turn_active'>, askable: Askable | null): boolean {
  if (!askable?.compacts) return false
  if (s.state === 'running') return s.turn_active !== 1
  return (isTerminal(s.state) || s.state === 'suspended') && askable.restorable
}
