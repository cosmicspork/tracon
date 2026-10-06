import type { ExternalLane } from './types'

const KEY = 'tracon.external.dismissed'

/** A lane's identity on the page: its channel and its label. */
export function laneKey(lane: Pick<ExternalLane, 'channel' | 'lane'>): string {
  return `${lane.channel}\u0000${lane.lane ?? ''}`
}

/** Lane key → the last call it had when it was dismissed. */
export type Dismissals = Record<string, number>

/** A dismissed lane comes back with its next call. */
export function visibleLanes(lanes: ExternalLane[], dismissed: Dismissals): ExternalLane[] {
  return lanes.filter((lane) => {
    const at = dismissed[laneKey(lane)]
    return at === undefined || lane.last_ms > at
  })
}

/** Dismissals for lanes that are gone from the view are dropped. */
export function prune(dismissed: Dismissals, lanes: ExternalLane[]): Dismissals {
  const keep = new Set(lanes.map(laneKey))
  return Object.fromEntries(Object.entries(dismissed).filter(([key]) => keep.has(key)))
}

export function loadDismissals(): Dismissals {
  try {
    const raw = localStorage.getItem(KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : {}
    return parsed && typeof parsed === 'object' ? (parsed as Dismissals) : {}
  } catch {
    return {}
  }
}

export function saveDismissals(dismissed: Dismissals) {
  try {
    localStorage.setItem(KEY, JSON.stringify(dismissed))
  } catch {
    // A private window keeps them for this page only.
  }
}

/** What the liveness dot says: the harness process, when it said which. */
export function liveness(lane: ExternalLane): 'running' | 'gone' | 'unknown' {
  if (lane.running == null) return 'unknown'
  return lane.running > 0 ? 'running' : 'gone'
}
