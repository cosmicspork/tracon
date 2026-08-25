// Typed wrappers over the node's API. Errors surface the node's message so the
// interface can say what the node said, not "request failed".

import type { Event, NodeInfo, Queue, Session } from './types'

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
  node: () => call<NodeInfo>('GET', '/api/node'),
  queue: () => call<Queue>('GET', '/api/queue'),
  sessions: () => call<Session[]>('GET', '/api/sessions'),
  session: (id: string) =>
    call<{ session: Session; waiting: unknown[] }>('GET', `/api/sessions/${id}`),
  events: (id: string, after = 0) =>
    call<Event[]>('GET', `/api/sessions/${id}/events?after=${after}&limit=2000`),
  createSession: (spec: {
    channel: string
    repo_path: string
    branch?: string
    work_item_id?: string
    model: string
    budget_tokens?: number
  }) => call<Session>('POST', '/api/sessions', spec),
  prompt: (id: string, text: string) => call<void>('POST', `/api/sessions/${id}/prompt`, { text }),
  kill: (id: string) => call<void>('POST', `/api/sessions/${id}/kill`),
  saveDraft: (id: string, text: string) => call<void>('PUT', `/api/sessions/${id}/draft`, { text }),
  answer: (permissionId: string, optionId: string) =>
    call<void>('POST', `/api/permissions/${permissionId}/answer`, { option_id: optionId }),
}
