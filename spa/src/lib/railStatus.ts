// The rail's two status lights. The colours follow the status cards: red is
// broken, yellow wants the operator, green is work going on. Idle is grey, not
// yellow, so a quiet node does not read as one needing attention. The banners
// say what is wrong; the light only says that something is.

import { formatAge } from './format'
import type { AwakeState, MeshState, NodeInfo, Session, SessionState } from './types'

export type Tone = 'crit' | 'wait' | 'ok' | 'idle'

export interface Light {
  tone: Tone
  /** The state in words, for the tooltip and screen readers. */
  state: string
  /** Everything else worth a hover: the node name, keep-awake, a recent sleep. */
  detail: string[]
}

const WORKING: SessionState[] = ['starting', 'running', 'waiting_on_check']

/** Sessions on this node with a harness doing something. */
export function runningHere(sessions: Iterable<Session>, nodeId: string | undefined): number {
  if (!nodeId) return 0
  let n = 0
  for (const s of sessions) if (s.node_id === nodeId && !s.archived_ms && WORKING.includes(s.state)) n++
  return n
}

export function nodeLight(o: {
  connected: boolean
  node: NodeInfo | null
  cannotRun: boolean
  waiting: number
  running: number
  awake: AwakeState | null
  now: number
}): Light {
  const detail: string[] = []
  if (o.node) detail.push(o.node.name)
  if (o.awake?.held) detail.push(`keeping awake: ${o.awake.reason ?? 'work in progress'}`)
  else if (o.awake?.error) detail.push(`cannot keep this machine awake: ${o.awake.error}`)
  const slept = o.awake?.last_suspend
  if (slept && o.now - slept.woke_ms < 3_600_000) {
    detail.push(`slept ${Math.round(slept.asleep_ms / 60_000)} min, woke ${formatAge(slept.woke_ms, o.now)} ago`)
  }
  const light = (tone: Tone, state: string): Light => ({ tone, state, detail })
  if (!o.connected) return light('crit', 'disconnected')
  if (o.node?.state === 'refused') return light('crit', 'failed its isolation check')
  if (o.cannotRun) return light('crit', 'cannot run tasks')
  if (o.waiting > 0) return light('wait', `${o.waiting} waiting on you`)
  if (o.running > 0) return light('ok', `${o.running} session${o.running === 1 ? '' : 's'} running`)
  return light('idle', 'idle')
}

/** Null when no hub is configured: the rail shows no hub line at all. */
export function hubLight(mesh: MeshState | null, now: number): Light | null {
  const hub = mesh?.hub
  if (!hub || hub.state === 'disabled') return null
  if (hub.state === 'connected') return { tone: 'ok', state: 'connected', detail: [] }
  return { tone: 'crit', state: `unreachable for ${formatAge(hub.since_ms, now)}`, detail: [] }
}
