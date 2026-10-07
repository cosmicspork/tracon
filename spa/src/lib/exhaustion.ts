// What the session view says about an exhausted provider: what refused, and
// what happens next. Only what the node recorded; a reset time is shown only
// when the provider sent one.

import type { SessionExhaustion } from './types'

export interface ExhaustionNote {
  title: string
  detail: string
}

function at(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  })
}

/** The banner for a session its provider's exhaustion is holding, or null. */
export function exhaustionNote(e: SessionExhaustion | null | undefined): ExhaustionNote | null {
  if (!e?.outcome) return null
  const title = `${e.provider ?? 'its provider'} is exhausted`
  switch (e.outcome) {
    case 'waiting':
      return {
        title,
        detail:
          e.next_wake_ms !== null
            ? `resumes when its limit resets, ${at(e.next_wake_ms)}`
            : 'resumes when its limit resets',
      }
    case 'falling_back':
      return { title, detail: `carrying on with ${e.fallback ?? 'the fallback'} from where it stopped` }
    case 'held':
      return { title, detail: e.note ? `waiting for you · ${e.note}` : 'waiting for you' }
    default:
      return null
  }
}
