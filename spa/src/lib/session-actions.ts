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
 * The prompt box: whether it takes typing, and what its placeholder says.
 * `reason` is why a prompt cannot be sent now; while a turn runs the box
 * still takes the next prompt, so it must not say input is disabled.
 */
export function composerState(reason: string | null, turnRunning: boolean): { disabled: boolean; placeholder: string } {
  if (reason === null) return { disabled: false, placeholder: 'Send a prompt. Drafts are held on the node.' }
  if (turnRunning) {
    const said = reason.charAt(0).toUpperCase() + reason.slice(1)
    return { disabled: false, placeholder: `${said}. Type the next prompt; Send opens when that clears.` }
  }
  return { disabled: true, placeholder: `Input disabled: ${reason}` }
}
