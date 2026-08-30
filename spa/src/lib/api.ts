// Typed wrappers over the node's API. Errors surface the node's message so the
// interface can say what the node said, not "request failed".

import type {
  ChannelInfo,
  ChannelMetrics,
  CredentialSummary,
  Document,
  Event,
  ForgeList,
  Invite,
  ManagedRepo,
  MeshState,
  NodeInfo,
  Promotion,
  PromotionItem,
  ProviderInfo,
  PushDevice,
  Queue,
  RecallHit,
  RecentRepo,
  Review,
  Session,
  WorkItem,
  WorkView,
} from './types'

/** A document edit that lost to another: the current state comes back. */
export class DocConflict extends Error {
  constructor(
    public hash: string,
    public body: string,
  ) {
    super('the document changed since it was read')
  }
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message)
  }
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body !== undefined ? { 'content-type': 'application/json' } : undefined,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  })
  const text = await res.text()
  // Not every failure body is JSON: axum's extractors reject with plain text.
  let json: { error?: { message?: string } } | null = null
  try {
    json = text ? JSON.parse(text) : null
  } catch {
    json = null
  }
  if (!res.ok) {
    const message = json?.error?.message ?? (text || `${res.status} ${res.statusText}`)
    throw new ApiError(res.status, message)
  }
  return json as T
}

export const api = {
  login: (token: string) => call<{ ok: boolean }>('POST', '/api/login', { token }),
  logout: () => call<{ ok: boolean }>('POST', '/api/logout'),
  node: () => call<NodeInfo>('GET', '/api/node'),
  nodes: () => call<NodeInfo[]>('GET', '/api/nodes'),
  mesh: () => call<MeshState>('GET', '/api/mesh'),
  channels: () => call<ChannelInfo[]>('GET', '/api/channels'),
  /** Dotted keys nest; `null` removes. Handed to every member of the channel. */
  putChannelBindings: (name: string, patch: Record<string, unknown>) =>
    call<{ name: string; bindings: Record<string, unknown> }>('PUT', `/api/channels/${name}/bindings`, patch),
  // Push: this node pushes to the phones subscribed here.
  pushKey: () => call<{ key: string }>('GET', '/api/push/key'),
  pushDevices: () => call<{ devices: PushDevice[] }>('GET', '/api/push/subscriptions'),
  putPushSubscription: (sub: unknown) => call<{ id: string }>('POST', '/api/push/subscriptions', sub),
  deletePushSubscription: (id: string) => call<void>('DELETE', `/api/push/subscriptions/${id}`),
  deletePushSubscriptionByEndpoint: (endpoint: string) =>
    call<void>('DELETE', '/api/push/subscriptions', { endpoint }),
  testPush: () => call<{ sent: { id: string; outcome: string }[] }>('POST', '/api/push/test', {}),
  queue: () => call<Queue>('GET', '/api/queue'),
  recentRepos: () =>
    call<{ repos: RecentRepo[]; managed: ManagedRepo[] }>('GET', '/api/repos/recent'),
  forgeRepos: (channel: string) =>
    call<{ forges: ForgeList[] }>('GET', `/api/forge/repos?channel=${encodeURIComponent(channel)}`),
  cloneRepo: (b: { channel: string; forge: string; host: string; owner: string; name: string }) =>
    call<{ repo_path: string }>('POST', '/api/repos/clone', b),
  sessions: () => call<Session[]>('GET', '/api/sessions'),
  session: (id: string) =>
    call<{ session: Session; waiting: unknown[] }>('GET', `/api/sessions/${id}`),
  // Page through the whole history: a long session has more events than one
  // request returns, and stopping at a fixed cap would show the oldest events
  // with a gap before the live tail.
  events: async (id: string): Promise<Event[]> => {
    const limit = 1000
    const all: Event[] = []
    let after = 0
    for (;;) {
      const batch = await call<Event[]>(
        'GET',
        `/api/sessions/${id}/events?after=${after}&limit=${limit}`,
      )
      all.push(...batch)
      if (batch.length < limit) break
      after = batch[batch.length - 1].seq
    }
    return all
  },
  createSession: (spec: {
    channel: string
    repo_path: string
    branch?: string
    work_item_id?: string
    model: string
    budget_tokens?: number
    node_id?: string
    phase?: 'plan' | 'execute'
  }) => call<Session>('POST', '/api/sessions', spec),
  prompt: (id: string, text: string) => call<void>('POST', `/api/sessions/${id}/prompt`, { text }),
  kill: (id: string) => call<void>('POST', `/api/sessions/${id}/kill`),
  saveDraft: (id: string, text: string) => call<void>('PUT', `/api/sessions/${id}/draft`, { text }),
  review: (id: string) => call<{ review: Review; stale: string[] }>('GET', `/api/reviews/${id}`),
  /** One reviewed file as it was submitted, for the diff editor. */
  reviewFile: (id: string, path: string) =>
    call<{ path: string; text: string | null }>(
      'GET',
      `/api/reviews/${id}/file?path=${encodeURIComponent(path)}`,
    ),
  decideReview: (
    id: string,
    verdict: {
      verdict: 'approve' | 'reject' | 'revise'
      reason?: string
      title?: string
      body?: string
      patch?: string
    },
  ) => call<{ state: string; published?: string }>('POST', `/api/reviews/${id}/verdict`, verdict),
  releaseReview: (id: string) => call<void>('POST', `/api/reviews/${id}/release`),
  answer: (permissionId: string, optionId: string) =>
    call<void>('POST', `/api/permissions/${permissionId}/answer`, { option_id: optionId }),
  // Documents: read by slug, search by content, edit with the hash last read.
  docs: (channel?: string, kind?: string) => {
    const q = new URLSearchParams()
    if (channel) q.set('channel', channel)
    if (kind) q.set('kind', kind)
    const s = q.toString()
    return call<{ docs: Document[] }>('GET', `/api/docs${s ? `?${s}` : ''}`)
  },
  searchDocs: (text: string, channel?: string) => {
    const q = new URLSearchParams({ q: text })
    if (channel) q.set('channel', channel)
    return call<{ hits: RecallHit[]; text_only?: boolean }>('GET', `/api/docs?${q}`)
  },
  doc: (channel: string, slug: string) => call<Document>('GET', `/api/docs/${channel}/${slug}`),
  putDoc: async (channel: string, slug: string, body: string, ifMatch?: string): Promise<Document> => {
    const res = await fetch(`/api/docs/${channel}/${slug}`, {
      method: 'PUT',
      headers: {
        'content-type': 'application/json',
        ...(ifMatch ? { 'if-match': ifMatch } : { 'if-none-match': '*' }),
      },
      body: JSON.stringify({ body }),
    })
    const text = await res.text()
    let json: { error?: { message?: string }; hash?: string; body?: string } | null = null
    try {
      json = text ? JSON.parse(text) : null
    } catch {
      json = null
    }
    if (res.status === 412) throw new DocConflict(json?.hash ?? '', json?.body ?? '')
    if (!res.ok) throw new ApiError(res.status, json?.error?.message ?? text)
    return json as unknown as Document
  },
  deleteDoc: (channel: string, slug: string) => call<void>('DELETE', `/api/docs/${channel}/${slug}`),
  // Promotion batches: read, decide per item, or build tonight's now.
  promotion: (id: string) =>
    call<{ promotion: Promotion; items: PromotionItem[]; verdicts: Record<string, string> }>(
      'GET',
      `/api/promotions/${id}`,
    ),
  decidePromotion: (id: string, verdicts: Record<string, 'promote' | 'reject'>) =>
    call<{ state: string }>('POST', `/api/promotions/${id}/verdict`, { verdicts }),
  batchPromotions: () => call<{ created: string[] }>('POST', '/api/promotions/batch'),
  // Providers: connect through the harness's own login, paste the code back.
  credentials: () => call<{ credentials: CredentialSummary[] }>('GET', '/api/credentials'),
  shareCredential: (name: string, to: string) =>
    call<{ shared: string; to: string }>('POST', `/api/credentials/${name}/share`, { to }),
  providers: () => call<ProviderInfo[]>('GET', '/api/providers'),
  connectProvider: (name: string, channels: string[]) =>
    call<{ name: string; url: string }>('POST', `/api/providers/${name}/connect`, { channels }),
  providerCode: (name: string, code: string) =>
    call<void>('POST', `/api/providers/${name}/code`, { code }),
  disconnectProvider: (name: string) => call<void>('POST', `/api/providers/${name}/disconnect`),
  // Node-scoped provider actions: the serving node runs them itself or seals
  // the command to the owner. Works for every node, this one included.
  nodeConnectProvider: (nodeId: string, name: string, channels: string[]) =>
    call<{ name: string; url: string }>(`POST`, `/api/nodes/${nodeId}/providers/${name}/connect`, {
      channels,
    }),
  nodeProviderCode: (nodeId: string, name: string, code: string) =>
    call<{ ok: boolean }>('POST', `/api/nodes/${nodeId}/providers/${name}/code`, { code }),
  nodeDisconnectProvider: (nodeId: string, name: string) =>
    call<{ ok: boolean }>('POST', `/api/nodes/${nodeId}/providers/${name}/disconnect`),
  // The work ledger.
  work: (channel: string, opts: { project_id?: string; state?: string } = {}) => {
    const q = new URLSearchParams({ channel })
    if (opts.project_id) q.set('project_id', opts.project_id)
    if (opts.state) q.set('state', opts.state)
    return call<{ items: WorkView[] }>('GET', `/api/work?${q}`)
  },
  workReady: (channel: string, project_id?: string) => {
    const q = new URLSearchParams({ channel })
    if (project_id) q.set('project_id', project_id)
    return call<{ items: WorkView[] }>('GET', `/api/work/ready?${q}`)
  },
  workItem: (id: string) =>
    call<{ item: WorkView | null; sessions: Session[]; discovered: { id: string; title: string; state: string }[] }>(
      'GET',
      `/api/work/${id}`,
    ),
  addWork: (w: { channel: string; title: string; body?: string; deps?: string[]; priority?: number; project_id?: string; discovered_from?: string }) =>
    call<WorkItem>('POST', '/api/work', w),
  putWork: (id: string, patch: { title?: string; body?: string; deps?: string[]; priority?: number; state?: 'open' | 'closed' }) =>
    call<WorkItem>('PUT', `/api/work/${id}`, patch),
  deleteWork: (id: string) => call<void>('DELETE', `/api/work/${id}`),
  metrics: (sinceMs?: number) =>
    call<{ since_ms: number; node_id: string; note: string; channels: ChannelMetrics[] }>(
      'GET',
      `/api/metrics${sinceMs ? `?since_ms=${sinceMs}` : ''}`,
    ),
  // Enrollment: browser only.
  openInvite: (channels: string[]) => call<Invite>('POST', '/api/mesh/invite', { channels }),
  pollInvite: (code: string) => call<Invite>('GET', `/api/mesh/invite/${code}`),
  admitInvite: (code: string) => call<Invite>('POST', `/api/mesh/invite/${code}/admit`),
  cancelInvite: (code: string) => call<void>('DELETE', `/api/mesh/invite/${code}`),
}
