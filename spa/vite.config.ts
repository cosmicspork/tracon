import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig, type Plugin, type PluginOption } from 'vite'

// `TRACON_FIXTURES=1 bun run dev` serves the interface against canned state
// instead of a node: fixtures/api.json keyed by path, `*` matching one
// segment, and every small *_ms value applied as an offset from now. This is
// how the README's screenshots are taken (scripts/screenshots.mjs) — no
// boundary, no hub, no credentials.
const fixtures = (): Plugin => ({
  name: 'tracon-fixtures',
  apply: 'serve',
  configureServer(server) {
    const table = JSON.parse(
      readFileSync(fileURLToPath(new URL('./fixtures/api.json', import.meta.url)), 'utf8'),
    ) as Record<string, unknown>
    const stamp = (v: unknown): unknown => {
      if (Array.isArray(v)) return v.map(stamp)
      if (v !== null && typeof v === 'object') {
        return Object.fromEntries(
          Object.entries(v as Record<string, unknown>).map(([k, val]) => [
            k,
            k.endsWith('_ms') && typeof val === 'number' && Math.abs(val) < 1e12
              ? Date.now() + val
              : stamp(val),
          ]),
        )
      }
      return v
    }
    server.middlewares.use((req, res, next) => {
      const url = (req.url ?? '').split('?')[0]
      if (!url.startsWith('/api/')) return next()
      if (url === '/api/stream') {
        res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' })
        res.write(':\n\n')
        return // held open; the store needs the connection, not events
      }
      const wild = url.split('/').map((p, i) => (i === 3 && url.startsWith('/api/sessions/') ? '*' : p)).join('/')
      const body = table[url] ?? table[wild]
      if (body === undefined) {
        res.writeHead(404, { 'content-type': 'application/json' })
        res.end(JSON.stringify({ error: { code: 404, message: `no fixture for ${url}` } }))
        return
      }
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify(stamp(body)))
    })
  },
})

const fixtureMode = Boolean(process.env.TRACON_FIXTURES)

// The release this bundle belongs to, so a page left open across a node
// upgrade can tell it is running the previous interface. Empty when built
// without the workspace manifest; the page then never claims to be stale.
const appVersion = (() => {
  try {
    const manifest = readFileSync(fileURLToPath(new URL('../Cargo.toml', import.meta.url)), 'utf8')
    return /\[workspace\.package\][^[]*?\bversion\s*=\s*"([^"]+)"/.exec(manifest)?.[1] ?? ''
  } catch {
    return ''
  }
})()

export default defineConfig({
  define: { __TRACON_VERSION__: JSON.stringify(appVersion) },
  plugins: [svelte(), fixtureMode ? fixtures() : undefined].filter(Boolean) as PluginOption[],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    // setup.html is what the desktop app opens on from its own bundle, before
    // a node exists to serve the interface.
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL('./index.html', import.meta.url)),
        setup: fileURLToPath(new URL('./setup.html', import.meta.url)),
      },
    },
  },
  server: fixtureMode
    ? {}
    : {
        proxy: {
          '/api': { target: 'http://127.0.0.1:7420', changeOrigin: false },
        },
      },
})
