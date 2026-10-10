import type { Session } from './types'

export type Tone = 'wait' | 'crit' | 'run' | 'ok' | 'dim'

// The colour a session's row carries. Amber is waiting on you and red is
// failure; an ended session is green only when its work landed. One the
// operator stopped, carried on elsewhere, or lost to a restart or an
// exhausted provider ended without failing and without finishing, so it
// takes the quiet treatment rather than claiming either.
export function sessionTone(session: Pick<Session, 'state' | 'end_reason'>): Tone {
  switch (session.state) {
    case 'waiting_on_you':
    case 'paused':
      return 'wait'
    case 'failed':
    case 'killed_budget':
      return 'crit'
    case 'closed':
      switch (session.end_reason) {
        case 'error':
        case 'incompatible':
        case 'budget':
          return 'crit'
        case 'killed_user':
        case 'detached':
        case 'continued':
        case 'node_restart':
        case 'provider_exhausted':
          return 'dim'
        default:
          return 'ok'
      }
    default:
      return 'run'
  }
}
