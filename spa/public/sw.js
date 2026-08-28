// The service worker exists to make the interface installable and to make it
// open fast. It is deliberately not an offline mode: sessions live in the node,
// and a phone showing a cached queue it cannot act on would be worse than a
// phone that says it cannot reach the node.
//
// So: the shell is cached, and nothing under /api ever is. There is no local
// replica and no credential here — the cookie is the browser's, not the
// worker's.

const VERSION = 'v1'
const SHELL = `tracon-shell-${VERSION}`
const ASSETS = `tracon-assets-${VERSION}`

// Enough to render the frame and ask the node for the rest.
const SHELL_URLS = ['/', '/manifest.webmanifest', '/icon-192.png']

self.addEventListener('install', (event) => {
  event.waitUntil(
    caches
      .open(SHELL)
      // A missing entry must not fail the install and leave the page
      // uncontrolled, so each is added on its own.
      .then((cache) => Promise.all(SHELL_URLS.map((u) => cache.add(u).catch(() => {}))))
      .then(() => self.skipWaiting()),
  )
})

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(keys.filter((k) => k !== SHELL && k !== ASSETS).map((k) => caches.delete(k))),
      )
      .then(() => self.clients.claim()),
  )
})

self.addEventListener('fetch', (event) => {
  const req = event.request
  if (req.method !== 'GET') return

  const url = new URL(req.url)
  if (url.origin !== self.location.origin) return

  // The node's API and its event stream are never cached and never even
  // intercepted: what is waiting on you is worth nothing if it is stale, and
  // wrapping an endless SSE response in a fetch handler breaks it outright.
  if (url.pathname.startsWith('/api/')) return

  // Build assets carry a content hash, so a hit is always correct.
  if (url.pathname.startsWith('/assets/')) {
    event.respondWith(
      caches.match(req).then(
        (hit) =>
          hit ||
          fetch(req).then((res) => {
            if (res.ok) {
              const copy = res.clone()
              caches.open(ASSETS).then((c) => c.put(req, copy))
            }
            return res
          }),
      ),
    )
    return
  }

  // Everything else is a navigation: the node serves the same shell for every
  // route. Prefer the network so a deployed build is picked up, and fall back
  // to the cached shell so a cold phone still opens.
  if (req.mode === 'navigate') {
    event.respondWith(
      fetch(req)
        .then((res) => {
          if (res.ok) {
            const copy = res.clone()
            caches.open(SHELL).then((c) => c.put('/', copy))
          }
          return res
        })
        .catch(() => caches.match('/').then((hit) => hit || Response.error())),
    )
    return
  }

  event.respondWith(caches.match(req).then((hit) => hit || fetch(req)))
})
