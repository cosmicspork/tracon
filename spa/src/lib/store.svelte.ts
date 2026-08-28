// The one store. State lives on the node; this is a cache fed by the stream.
// On reconnect everything is refetched and ephemeral state dropped, so a client
// crash costs nothing (the client crash invariant).

import { api, type ChannelInfo } from './api'
import { upsertNode } from './nodes'
import type {
  Event,
  Frame,
  MeshState,
  NodeInfo,
  Permission,
  ProviderInfo,
  Queue,
  Review,
  Session,
} from './types'

const MAX_LIVE_EVENTS = 3000
const RECONNECTED_BANNER_MS = 8000

class Store {
  /** Every node this one knows, itself first. */
  nodes = $state<NodeInfo[]>([])
  /** The hub's reachability; null until the first fetch. */
  mesh = $state<MeshState | null>(null)
  /** Channels this node can start sessions on, and who is bound to each. */
  channels = $state<ChannelInfo[]>([])
  /** Model providers on the serving node and whether each is connected. */
  providers = $state<ProviderInfo[]>([])
  /** Bumped when a document changes anywhere on the mesh; screens refetch on it. */
  docsVersion = $state(0)
  /** Bumped when a work item changes anywhere on the mesh. */
  workVersion = $state(0)
  /** Set briefly after the hub comes back: how many queued items went out. */
  reconnected = $state<number | null>(null)
  queue = $state<Queue>({ waiting: [], reviews: [], promotions: [], running: [], ended: [] })
  sessions = $state<Map<string, Session>>(new Map())
  /** Persisted events for the session that is open on screen. */
  events = $state<Event[]>([])
  openSession = $state<string | null>(null)
  /** Live text not yet persisted, keyed by messageId. */
  openChunks = $state<Map<string, { kind: string; text: string }>>(new Map())
  /** Latest ephemeral status per tool call. */
  toolProgress = $state<Map<string, string>>(new Map())
  connected = $state(false)
  lastSeq = 0

  private source: EventSource | null = null
  private wasConnected = false
  private reconnectTimer: ReturnType<typeof setTimeout> | undefined

  /** The node that served this interface. */
  get node(): NodeInfo | null {
    return this.nodes.find((n) => n.is_self) ?? null
  }

  connect() {
    if (this.source) return
    void this.refetch()
    // EventSource resends Last-Event-ID on its own reconnects; the node replays
    // persisted events after it.
    this.source = new EventSource('/api/stream')
    this.source.onopen = () => {
      this.connected = true
      if (this.wasConnected) {
        // Frames may have been missed while down: refetch snapshots and drop
        // ephemeral state, which the replayed events supersede.
        this.openChunks = new Map()
        this.toolProgress = new Map()
        void this.refetch()
      }
      this.wasConnected = true
    }
    this.source.onerror = () => {
      this.connected = false
    }
    for (const name of [
      'event',
      'chunk',
      'tool_update',
      'session',
      'queue',
      'reviews',
      'node',
      'mesh',
      'providers',
      'promotions',
      'changes',
    ] as const) {
      this.source.addEventListener(name, (m) => this.onFrame(JSON.parse((m as MessageEvent).data)))
    }
  }

  async refetch() {
    try {
      const [nodes, mesh, channels, queue, sessions, providers] = await Promise.all([
        api.nodes(),
        api.mesh(),
        api.channels(),
        api.queue(),
        api.sessions(),
        api.providers().catch(() => [] as ProviderInfo[]),
      ])
      this.nodes = nodes.reduce(upsertNode, [] as NodeInfo[])
      this.mesh = mesh
      this.channels = channels
      this.providers = providers
      this.queue = queue
      this.sessions = new Map(sessions.map((s) => [s.id, s]))
      if (this.openSession) await this.loadEvents(this.openSession)
    } catch {
      // The node is unreachable; the stream's error handler shows it.
    }
  }

  async open(sessionId: string) {
    this.openSession = sessionId
    this.events = []
    this.openChunks = new Map()
    this.toolProgress = new Map()
    await this.loadEvents(sessionId)
  }

  close() {
    this.openSession = null
    this.events = []
  }

  private async loadEvents(sessionId: string) {
    const events = await api.events(sessionId)
    if (this.openSession !== sessionId) return
    this.events = events
    for (const e of events) this.lastSeq = Math.max(this.lastSeq, e.seq)
  }

  private onFrame(frame: Frame) {
    switch (frame.type) {
      case 'event': {
        this.lastSeq = Math.max(this.lastSeq, frame.seq)
        if (frame.session_id === this.openSession) {
          if (!this.events.some((e) => e.seq === frame.seq)) {
            // Mirrored history can arrive out of order (a backfill lands after
            // live events); keep the log sorted by the owner's clock.
            this.events = [...this.events, frame]
              .sort((a, b) => a.at_ms - b.at_ms || a.seq - b.seq)
              .slice(-MAX_LIVE_EVENTS)
          }
          // The persisted event supersedes the live buffer for its message.
          if (frame.kind === 'message' || frame.kind === 'thought') {
            const next = new Map(this.openChunks)
            next.delete(frame.ref_id ?? '')
            next.delete('')
            this.openChunks = next
          }
        }
        break
      }
      case 'chunk': {
        if (frame.session_id !== this.openSession) break
        const key = frame.message_id ?? ''
        const next = new Map(this.openChunks)
        const open = next.get(key)
        next.set(key, { kind: frame.kind, text: (open?.text ?? '') + frame.text })
        this.openChunks = next
        break
      }
      case 'tool_update': {
        if (frame.session_id !== this.openSession) break
        const next = new Map(this.toolProgress)
        next.set(frame.tool_call_id, frame.status ?? '')
        this.toolProgress = next
        break
      }
      case 'session': {
        const next = new Map(this.sessions)
        next.set(frame.id, frame)
        this.sessions = next
        this.syncQueueSession(frame)
        break
      }
      case 'queue': {
        this.queue = { ...this.queue, waiting: frame.waiting }
        break
      }
      case 'reviews': {
        this.queue = { ...this.queue, reviews: frame.waiting }
        break
      }
      case 'node': {
        // A refreshed node (new model list, a refused state, a peer arriving or
        // dimming) reaches a live client without a reload. The frame carries a
        // `type` field the NodeInfo shape ignores.
        this.nodes = upsertNode(this.nodes, frame)
        break
      }
      case 'mesh': {
        const wasDown = this.mesh?.hub.state === 'unreachable'
        this.mesh = frame
        if (wasDown && frame.hub.state === 'connected') this.showReconnected(frame.queued)
        break
      }
      case 'providers': {
        this.providers = frame.providers
        break
      }
      case 'promotions': {
        this.queue = { ...this.queue, promotions: frame.waiting }
        break
      }
      case 'changes': {
        if (frame.changes.some((c) => c.table === 'document')) this.docsVersion += 1
        if (frame.changes.some((c) => c.table === 'work_item')) this.workVersion += 1
        break
      }
    }
  }

  private showReconnected(delivered: number) {
    this.reconnected = delivered
    clearTimeout(this.reconnectTimer)
    this.reconnectTimer = setTimeout(() => (this.reconnected = null), RECONNECTED_BANNER_MS)
  }

  private syncQueueSession(s: Session) {
    const strip = (list: Session[]) => list.filter((x) => x.id !== s.id)
    const terminal = ['closed', 'killed_budget', 'failed'].includes(s.state)
    this.queue = {
      waiting: this.queue.waiting,
      reviews: this.queue.reviews,
      promotions: this.queue.promotions,
      running: terminal ? strip(this.queue.running) : upsert(this.queue.running, s),
      ended: terminal ? upsert(this.queue.ended, s) : strip(this.queue.ended),
    }
  }

  waitingFor(sessionId: string): Permission[] {
    return this.queue.waiting.filter((p) => p.session_id === sessionId)
  }

  reviewsFor(sessionId: string): Review[] {
    return this.queue.reviews.filter((r) => r.session_id === sessionId)
  }
}

function upsert(list: Session[], s: Session): Session[] {
  const i = list.findIndex((x) => x.id === s.id)
  if (i === -1) return [s, ...list]
  const next = [...list]
  next[i] = s
  return next
}

export const store = new Store()
