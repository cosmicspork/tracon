// What a session's page offers, from the state the node recorded. Each
// action is offered only where the node would take it: Pause needs a running
// supervisor, Resume a paused one, and a suspended session has neither — its
// container is stopped, and the way on is Continue, in its banner.

import { isTerminal, type Session } from './types'

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
