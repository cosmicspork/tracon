// Typed wrappers over the node's API. Errors surface the node's message so the
// interface can say what the node said, not "request failed".

import type { Event, Invite, MeshState, NodeInfo, ProviderInfo, Queue, Review, Session } from './types'

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

export interface ChannelInfo {
  name: string
  nodes: string[]
}

export const api = {
  node: () => call<NodeInfo>('GET', '/api/node'),
  nodes: () => call<NodeInfo[]>('GET', '/api/nodes'),
  mesh: () => call<MeshState>('GET', '/api/mesh'),
  channels: () => call<ChannelInfo[]>('GET', '/api/channels'),
  queue: () => call<Queue>('GET', '/api/queue'),
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
  }) => call<Session>('POST', '/api/sessions', spec),
  prompt: (id: string, text: string) => call<void>('POST', `/api/sessions/${id}/prompt`, { text }),
  kill: (id: string) => call<void>('POST', `/api/sessions/${id}/kill`),
  saveDraft: (id: string, text: string) => call<void>('PUT', `/api/sessions/${id}/draft`, { text }),
  review: (id: string) => call<{ review: Review; stale: string[] }>('GET', `/api/reviews/${id}`),
  decideReview: (
    id: string,
    verdict: {
      verdict: 'approve' | 'reject' | 'revise'
      reason?: string
      title?: string
      body?: string
    },
  ) => call<{ state: string; published?: string }>('POST', `/api/reviews/${id}/verdict`, verdict),
  releaseReview: (id: string) => call<void>('POST', `/api/reviews/${id}/release`),
  answer: (permissionId: string, optionId: string) =>
    call<void>('POST', `/api/permissions/${permissionId}/answer`, { option_id: optionId }),
  // Providers: connect through the harness's own login, paste the code back.
  providers: () => call<ProviderInfo[]>('GET', '/api/providers'),
  connectProvider: (name: string, channels: string[]) =>
    call<{ name: string; url: string }>('POST', `/api/providers/${name}/connect`, { channels }),
  providerCode: (name: string, code: string) =>
    call<void>('POST', `/api/providers/${name}/code`, { code }),
  disconnectProvider: (name: string) => call<void>('POST', `/api/providers/${name}/disconnect`),
  // Enrollment: browser only.
  openInvite: (channels: string[]) => call<Invite>('POST', '/api/mesh/invite', { channels }),
  pollInvite: (code: string) => call<Invite>('GET', `/api/mesh/invite/${code}`),
  admitInvite: (code: string) => call<Invite>('POST', `/api/mesh/invite/${code}/admit`),
  cancelInvite: (code: string) => call<void>('DELETE', `/api/mesh/invite/${code}`),
}
