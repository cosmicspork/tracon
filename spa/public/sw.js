// The service worker exists to make the interface installable and to make it
// open fast. It is deliberately not an offline mode: sessions live in the node,
// and a phone showing a cached queue it cannot act on would be worse than a
// phone that says it cannot reach the node.
//
// So: the shell is cached, and nothing under /api ever is. There is no local
// replica and no credential here — the cookie is the browser's, not the
// worker's.
//
// It is also where a push lands. The node seals each push to this device's
// key; the browser opens it and hands the worker a small JSON with a title, a
// body, a tag and a path on this origin.

const VERSION = 'v2'
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

// Every push shows something. iOS revokes a subscription after a few pushes
// that show nothing, so a payload that will not parse still gets a banner.
self.addEventListener('push', (event) => {
  let data = null
  try {
    data = event.data ? event.data.json() : null
  } catch {
    data = null
  }
  const title = (data && data.title) || 'tracon'
  const body = (data && data.body) || 'Something is waiting on you.'
  const tag = (data && data.tag) || 'tracon-generic'
  const path = (data && data.path) || '/'
  event.waitUntil(
    self.registration.showNotification(title, {
      body,
      tag,
      // A tag replaces the banner with the same tag: two nodes announcing the
      // same approval read as one, and a summary replaces the last summary.
      renotify: false,
      icon: '/icon-192.png',
      data: { path },
    }),
  )
})

// Tapping lands on the item. An open window is reused and told where to go;
// otherwise one is opened. Either way the path is resolved against this
// origin, which is the node the subscription belongs to.
self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  const path = (event.notification.data && event.notification.data.path) || '/'
  const url = new URL(path, self.location.origin).href
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((wins) => {
      const win = wins.find((w) => new URL(w.url).origin === self.location.origin)
      if (win) {
        win.postMessage({ type: 'navigate', path })
        return win.focus ? win.focus() : undefined
      }
      return self.clients.openWindow(url)
    }),
  )
})

// The push service rotated the subscription: subscribe again with the same
// key and tell the node, so the device does not silently go quiet.
self.addEventListener('pushsubscriptionchange', (event) => {
  const key = event.oldSubscription && event.oldSubscription.options
    ? event.oldSubscription.options.applicationServerKey
    : undefined
  event.waitUntil(
    self.registration.pushManager
      .subscribe({ userVisibleOnly: true, applicationServerKey: key })
      .then((sub) =>
        fetch('/api/push/subscriptions', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(sub.toJSON()),
        }),
      )
      .catch(() => {}),
  )
})
