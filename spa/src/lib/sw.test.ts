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

  const shown: { title: string; options: Record<string, unknown> }[] = []
  const opened: string[] = []
  const posted: unknown[] = []
  const windows: { url: string; focused: boolean; postMessage: (m: unknown) => void; focus: () => void }[] = []
  const subscribed: unknown[] = []

  const self = {
    addEventListener: (name: string, fn: Handler) => {
      handlers[name] = fn
    },
    location: { origin: 'http://n' },
    skipWaiting: () => {},
    clients: {
      claim: async () => {},
      matchAll: async () => windows,
      openWindow: async (u: string) => {
        opened.push(u)
      },
    },
    registration: {
      showNotification: async (title: string, options: Record<string, unknown>) => {
        shown.push({ title, options })
      },
      pushManager: {
        subscribe: async (opts: unknown) => {
          subscribed.push(opts)
          return { toJSON: () => ({ endpoint: 'https://push.example/new', keys: { p256dh: 'k', auth: 'a' } }) }
        },
      },
    },
  }

  new Function('self', 'caches', 'fetch', 'Response', code)(self, caches, fetchStub, Response)
  return {
    handlers,
    caches,
    store,
    fetched: () => fetched,
    shown,
    opened,
    posted,
    windows,
    subscribed,
    openWindow: (url: string) => {
      const w = {
        url,
        focused: false,
        postMessage: (m: unknown) => posted.push(m),
        focus: () => {
          w.focused = true
        },
      }
      windows.push(w)
      return w
    },
  }
}

/** Fire a push with this payload (or none) and wait for the banner. */
async function push(sw: ReturnType<typeof load>, payload: unknown | null) {
  let done: unknown
  const event = {
    data:
      payload === null
        ? null
        : {
            json: () => {
              if (typeof payload === 'string') throw new Error('not json')
              return payload
            },
          },
    waitUntil: (p: unknown) => {
      done = p
    },
  }
  ;(sw.handlers.push as unknown as (e: unknown) => void)(event)
  await done
}

async function click(sw: ReturnType<typeof load>, path: string | undefined) {
  let done: unknown
  let closed = false
  const event = {
    notification: {
      data: path === undefined ? undefined : { path },
      close: () => {
        closed = true
      },
    },
    waitUntil: (p: unknown) => {
      done = p
    },
  }
  ;(sw.handlers.notificationclick as unknown as (e: unknown) => void)(event)
  await done
  return closed
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

describe('a push always shows something', () => {
  test('the payload becomes the banner, tagged so a duplicate replaces it', async () => {
    const sw = load()
    await push(sw, { title: 'Approval — feat/x', body: 'run just test', tag: 'tracon-perm-1', path: '/sessions/s1' })
    expect(sw.shown).toHaveLength(1)
    expect(sw.shown[0].title).toBe('Approval — feat/x')
    expect(sw.shown[0].options.body).toBe('run just test')
    expect(sw.shown[0].options.tag).toBe('tracon-perm-1')
    expect(sw.shown[0].options.renotify).toBe(false)
    expect(sw.shown[0].options.data).toEqual({ path: '/sessions/s1' })
  })

  test('no payload, or one that will not parse, still shows a banner', async () => {
    // iOS revokes a subscription whose pushes show nothing.
    const sw = load()
    await push(sw, null)
    await push(sw, 'not json')
    expect(sw.shown).toHaveLength(2)
    for (const s of sw.shown) {
      expect(s.title).toBe('tracon')
      expect(s.options.tag).toBe('tracon-generic')
      expect(s.options.data).toEqual({ path: '/' })
    }
  })
})

describe('tapping a banner lands on the item', () => {
  test('an open window is reused and told where to go', async () => {
    const sw = load()
    const w = sw.openWindow('http://n/queue')
    const closed = await click(sw, '/reviews/r1')
    expect(closed).toBe(true)
    expect(w.focused).toBe(true)
    expect(sw.posted).toEqual([{ type: 'navigate', path: '/reviews/r1' }])
    expect(sw.opened).toEqual([])
  })

  test('a window on another origin is not ours; a new one opens on this origin', async () => {
    const sw = load()
    sw.openWindow('https://elsewhere.example/')
    await click(sw, '/sessions/s1')
    expect(sw.opened).toEqual(['http://n/sessions/s1'])
    expect(sw.posted).toEqual([])
  })

  test('a banner without a path opens the front page', async () => {
    const sw = load()
    await click(sw, undefined)
    expect(sw.opened).toEqual(['http://n/'])
  })
})

test('a rotated subscription is re-registered with the node', async () => {
  const sw = load()
  const calls: { url: string; body: string }[] = []
  const fetchStub = async (url: string, init: { body: string }) => {
    calls.push({ url, body: init.body })
    return new Response('{}')
  }
  // The handler reaches fetch through the global the worker was loaded with;
  // re-load with a recording one.
  const code = readFileSync(new URL('../../public/sw.js', import.meta.url), 'utf8')
  const handlers: Record<string, (e: unknown) => void> = {}
  const self = {
    addEventListener: (n: string, f: (e: unknown) => void) => {
      handlers[n] = f
    },
    location: { origin: 'http://n' },
    registration: {
      pushManager: {
        subscribe: async () => ({ toJSON: () => ({ endpoint: 'https://push.example/new', keys: { p256dh: 'k', auth: 'a' } }) }),
      },
    },
  }
  new Function('self', 'caches', 'fetch', 'Response', code)(self, {}, fetchStub, Response)
  let done: unknown
  handlers.pushsubscriptionchange({
    oldSubscription: { options: { applicationServerKey: 'key' } },
    waitUntil: (p: unknown) => {
      done = p
    },
  })
  await done
  expect(calls).toHaveLength(1)
  expect(calls[0].url).toBe('/api/push/subscriptions')
  expect(JSON.parse(calls[0].body).endpoint).toBe('https://push.example/new')
  void sw
})
