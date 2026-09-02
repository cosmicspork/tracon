// Where a session belongs on the home when its row arrives over the stream.
// Pure, because the rule has three cases and each one has been wrong at least
// once: a session that ended moves lists, a session that is archived leaves
// both, and a session that is neither is updated in place.

import type { Queue, Session } from './types'

const TERMINAL = ['closed', 'killed_budget', 'failed']

export function isTerminalState(state: string): boolean {
  return TERMINAL.includes(state)
}

function upsert(list: Session[], s: Session): Session[] {
  const i = list.findIndex((x) => x.id === s.id)
  if (i === -1) return [s, ...list]
  const next = [...list]
  next[i] = s
  return next
}

/** The queue after one session frame. */
export function applySessionFrame(queue: Queue, s: Session): Queue {
  const strip = (list: Session[]) => list.filter((x) => x.id !== s.id)
  // Put away is put away: an archived session is on neither list, whatever
  // state it is in, or the next frame would bring it back.
  if (s.archived_ms) {
    return { ...queue, running: strip(queue.running), ended: strip(queue.ended) }
  }
  const terminal = isTerminalState(s.state)
  return {
    ...queue,
    running: terminal ? strip(queue.running) : upsert(queue.running, s),
    ended: terminal ? upsert(queue.ended, s) : strip(queue.ended),
  }
}
