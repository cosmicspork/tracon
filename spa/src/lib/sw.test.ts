// The service worker is plain JS served as-is, so it is exercised here by
// loading it against stub globals and asking what it does with a request.
// The rule that matters most is the one about /api: a cached approval queue
// would be worse than no queue at all.

import { expect, test, describe } from 'bun:test'
import { readFileSync } from 'node:fs'

type Handler = (event: FetchEventLike) => void

interface FetchEventLike {
  request: Request
  respondWith: (r: unknown) => void
  waitUntil: (p: unknown) => void
}

/** Load sw.js with fakes in place of the worker globals. */
function load() {
  const code = readFileSync(new URL('../../public/sw.js', import.meta.url), 'utf8')
  const handlers: Record<string, Handler> = {}
  const store = new Map<string, Map<string, Response>>()

  const cache = (name: string) => {
    if (!store.has(name)) store.set(name, new Map())
    const m = store.get(name)!
    return {
      add: async (u: string) => m.set(new URL(u, 'http://n').pathname, new Response('x')),
      put: async (req: Request | string, res: Response) =>
        m.set(new URL(typeof req === 'string' ? req : req.url, 'http://n').pathname, res),
      keys: async () => [...m.keys()].map((k) => new Request(`http://n${k}`)),
      match: async (req: Request | string) =>
        m.get(new URL(typeof req === 'string' ? req : req.url, 'http://n').pathname),
    }
  }

  const caches = {
    open: async (n: string) => cache(n),
    keys: async () => [...store.keys()],
    delete: async (n: string) => store.delete(n),
    match: async (req: Request | string) => {
      for (const n of store.keys()) {
        const hit = await cache(n).match(req)
        if (hit) return hit
      }
      return undefined
    },
  }

  let fetched = 0
  const fetchStub = async (req: Request | string) => {
    fetched++
    const url = typeof req === 'string' ? req : req.url
    return new Response(`network:${url}`, { status: 200 })
  }

  const self = {
    addEventListener: (name: string, fn: Handler) => {
      handlers[name] = fn
    },
    location: { origin: 'http://n' },
    skipWaiting: () => {},
    clients: { claim: async () => {} },
  }

  new Function('self', 'caches', 'fetch', 'Response', code)(self, caches, fetchStub, Response)
  return { handlers, caches, store, fetched: () => fetched }
}

/** Run the fetch handler and report whether it answered, and with what. */
async function handle(
  sw: ReturnType<typeof load>,
  url: string,
  init: { method?: string; mode?: string } = {},
) {
  // A plain object rather than a Request: `mode` is read-only on the real
  // thing, and the worker only ever reads these three fields.
  const req = { method: init.method ?? 'GET', url: `http://n${url}`, mode: init.mode }
  let responded: unknown
  let answered = false
  sw.handlers.fetch({
    request: req as Request,
    respondWith: (r) => {
      answered = true
      responded = r
    },
    waitUntil: () => {},
  })
  return { answered, response: answered ? await (responded as Promise<Response>) : undefined }
}

describe('the API is never touched', () => {
  test.each([
    '/api/queue',
    '/api/stream',
    '/api/node',
    '/api/reviews/abc',
    '/api/sessions/s1/events',
  ])('%s passes straight through', async (path) => {
    const sw = load()
    const { answered } = await handle(sw, path)
    expect(answered).toBe(false)
  })

  test('nothing under /api is ever put in a cache', async () => {
    const sw = load()
    await handle(sw, '/api/queue')
    await handle(sw, '/api/stream')
    const cached = [...sw.store.values()].flatMap((m) => [...m.keys()])
    expect(cached.filter((p) => p.startsWith('/api'))).toEqual([])
  })
})

test('a write is left alone even outside the API', async () => {
  const sw = load()
  const { answered } = await handle(sw, '/anything', { method: 'POST' })
  expect(answered).toBe(false)
})

test('another origin is not this worker s business', async () => {
  const sw = load()
  let answered = false
  sw.handlers.fetch({
    request: new Request('https://elsewhere.example/x'),
    respondWith: () => {
      answered = true
    },
    waitUntil: () => {},
  })
  expect(answered).toBe(false)
})

test('a hashed asset is served from the cache the second time', async () => {
  const sw = load()
  const first = await handle(sw, '/assets/index-abc123.js')
  expect(first.answered).toBe(true)
  expect(await first.response!.text()).toContain('network:')
  const before = sw.fetched()

  const second = await handle(sw, '/assets/index-abc123.js')
  expect(second.answered).toBe(true)
  expect(sw.fetched()).toBe(before)
})

test('a navigation prefers the network so a new build is picked up', async () => {
  const sw = load()
  const { answered, response } = await handle(sw, '/reviews/abc', { mode: 'navigate' })
  expect(answered).toBe(true)
  expect(await response!.text()).toContain('network:')
})

test('the shell is precached so a cold start has something to open', async () => {
  const sw = load()
  let waited: Promise<unknown> | undefined
  sw.handlers.install({
    request: undefined as never,
    respondWith: () => {},
    waitUntil: (p) => {
      waited = p as Promise<unknown>
    },
  })
  await waited
  const cached = [...sw.store.values()].flatMap((m) => [...m.keys()])
  expect(cached).toContain('/')
  expect(cached).toContain('/manifest.webmanifest')
})

test('an old version s caches are dropped on activate', async () => {
  const sw = load()
  await (await sw.caches.open('tracon-shell-v0')).put('/', new Response('stale'))
  let waited: Promise<unknown> | undefined
  sw.handlers.activate({
    request: undefined as never,
    respondWith: () => {},
    waitUntil: (p) => {
      waited = p as Promise<unknown>
    },
  })
  await waited
  expect(await sw.caches.keys()).not.toContain('tracon-shell-v0')
})
