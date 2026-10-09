// Every screen in every state worth looking at, for a visual audit before a
// release. Same fixture mode as scripts/screenshots.mjs (no node, no boundary),
// plus per-state API overrides and interactions from fixtures/states/*.mjs.
//
//   cd spa && bun scripts/ui-audit.mjs [--only review,settings] [--state failed]
//     [--sizes desktop,phone] [--schemes dark,light] [--out DIR] [--list]
//
// Writes DIR/<state>--<size>-<scheme>.png and DIR/manifest.json, which records
// for each state the API calls that had no fixture and any console or page
// errors: a screen that looks wrong because its fixture is missing says so.
//
// A state module default-exports an array of:
//
//   {
//     id: 'review-publish-failed',      // unique, kebab-case; the file name
//     area: 'review',                   // groups states; --only filters on it
//     route: '/reviews/r-1',
//     title: 'Publish failed, with recovery',
//     note: 'what to look at',          // optional
//     since: '#381',                    // optional: the change that added it
//     api: {                            // optional, checked before api.json
//                                       // (a function returning a promise that
//                                       // never settles holds a loading state)
//       'GET /api/reviews/r-1': { ... },        // body, `*` matches one segment
//       '/api/node': { ... },                   // no method means GET
//       'POST /api/x': { status: 409, body: { error: { code: 409, message: '…' } } },
//       '/api/y': (req) => ({ ... }),           // req: { method, path, query, body }
//     },
//     init: () => { ... },              // optional, runs in the page before its scripts
//     act: async (page) => { ... },     // optional, after load and before the shot
//     sizes: ['desktop'],               // optional subset
//     full: false,                      // optional: viewport only, not the whole page
//   }
//
// A response wrapped as { status, body } is sent with that status; anything
// else is a 200 with the value as JSON. Small *_ms numbers are offsets from
// now, as in api.json.

import { spawn } from 'node:child_process'
import { mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { fileURLToPath, pathToFileURL } from 'node:url'

import { chromium } from 'playwright-core'

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`)
  return i === -1 ? fallback : process.argv[i + 1]
}
const list = (v) => (v ? v.split(',').map((s) => s.trim()).filter(Boolean) : null)

const spa = fileURLToPath(new URL('..', import.meta.url))
const out = arg('out', fileURLToPath(new URL('../../target/ui-audit/', import.meta.url))).replace(/\/?$/, '/')
const onlyAreas = list(arg('only'))
const stateFilter = arg('state')
const sizeNames = list(arg('sizes')) ?? ['desktop', 'phone']
const schemes = list(arg('schemes')) ?? ['dark', 'light']
const workers = Number(arg('workers', '4'))
const port = Number(arg('port', '5198'))
const base = `http://127.0.0.1:${port}`
// Long pages are cut here; the rest is rarely different and makes the files huge.
const maxHeight = 6000

const sizes = {
  desktop: { width: 1280, height: 800 },
  phone: { width: 390, height: 844 },
}

const stateDir = new URL('../fixtures/states/', import.meta.url)
let states = []
for (const file of readdirSync(stateDir).filter((f) => f.endsWith('.mjs')).sort()) {
  const mod = await import(pathToFileURL(fileURLToPath(new URL(file, stateDir))).href)
  for (const s of mod.default) states.push({ ...s, file })
}
const seen = new Set()
for (const s of states) {
  if (!s.id || !s.area || !s.route || !s.title) throw new Error(`${s.file}: state needs id, area, route, title`)
  if (seen.has(s.id)) throw new Error(`${s.file}: duplicate state id ${s.id}`)
  seen.add(s.id)
}
if (onlyAreas) states = states.filter((s) => onlyAreas.includes(s.area))
if (stateFilter) states = states.filter((s) => s.id.includes(stateFilter))

if (process.argv.includes('--list')) {
  for (const s of states) console.log(`${s.area.padEnd(12)} ${s.id.padEnd(44)} ${s.route}`)
  process.exit(0)
}

// Durations end in _ms too, and are not offsets from now.
const durations = new Set(['asleep_ms', 'duration_ms', 'idle_ms', 'elapsed_ms', 'ttl_ms', 'timeout_ms', 'wall_ms', 'awake_ms', 'waiting_ms'])
const stamp = (v) => {
  if (Array.isArray(v)) return v.map(stamp)
  if (v !== null && typeof v === 'object') {
    return Object.fromEntries(
      Object.entries(v).map(([k, val]) => [
        k,
        k.endsWith('_ms') && !durations.has(k) && typeof val === 'number' && Math.abs(val) < 1e12
          ? Date.now() + val
          : stamp(val),
      ]),
    )
  }
  return v
}

/** The override for this request, or undefined: exact key first, then `*` segments. */
function lookup(api, method, path) {
  if (!api) return undefined
  const keys = Object.keys(api)
  const parse = (k) => {
    const m = /^([A-Z]+) (.*)$/.exec(k)
    return m ? { method: m[1], path: m[2] } : { method: 'GET', path: k }
  }
  const segs = path.split('/')
  let wild
  for (const k of keys) {
    const p = parse(k)
    if (p.method !== method) continue
    if (p.path === path) return api[k]
    const ps = p.path.split('/')
    if (wild === undefined && ps.length === segs.length && ps.every((x, i) => x === '*' || x === segs[i])) wild = api[k]
  }
  return wild
}

async function respond(route, value, req) {
  if (typeof value === 'function') value = await value(req)
  const wrapped = value !== null && typeof value === 'object' && !Array.isArray(value) && 'status' in value && 'body' in value && Object.keys(value).length === 2
  const status = wrapped ? value.status : 200
  const body = wrapped ? value.body : value
  await route.fulfill({
    status,
    contentType: typeof body === 'string' ? 'text/plain' : 'application/json',
    body: typeof body === 'string' ? body : JSON.stringify(stamp(body ?? null)),
  })
}

async function shoot(browser, state, sizeName, scheme) {
  const size = sizes[sizeName]
  const ctx = await browser.newContext({ viewport: size, colorScheme: scheme, deviceScaleFactor: 1 })
  const page = await ctx.newPage()
  const misses = new Set()
  const errors = []
  page.on('console', (m) => {
    if (m.type() === 'error' && !/Failed to load resource/.test(m.text())) errors.push(m.text().slice(0, 400))
  })
  page.on('pageerror', (e) => errors.push(`pageerror: ${String(e.message ?? e).slice(0, 400)}`))
  if (state.init) await page.addInitScript(state.init)
  await page.route('**/api/**', async (route) => {
    const req = route.request()
    const url = new URL(req.url())
    if (url.pathname === '/api/stream') return route.fallback()
    let body = null
    try {
      body = req.postDataJSON()
    } catch {
      body = req.postData()
    }
    const value = lookup(state.api, req.method(), url.pathname)
    if (value !== undefined) {
      return respond(route, value, { method: req.method(), path: url.pathname, query: Object.fromEntries(url.searchParams), body })
    }
    const res = await route.fetch()
    if (res.status() === 404) misses.add(`${req.method()} ${url.pathname}`)
    return route.fulfill({ response: res })
  })
  const file = `${state.id}--${sizeName}-${scheme}.png`
  try {
    await page.goto(`${base}${state.route}`, { waitUntil: 'load', timeout: 15000 })
    // Bounded: a state that holds a request open on purpose (a loading state)
    // never goes idle.
    await page.waitForLoadState('networkidle', { timeout: 4000 }).catch(() => {})
    await page.waitForTimeout(350)
    if (state.act) {
      await state.act(page, { size: sizeName, scheme })
      await page.waitForTimeout(350)
    }
    // A whole page is taken by growing the window to fit it, not with
    // fullPage: that stitches past the viewport, leaving the sticky rail
    // 800px tall and the phone's fixed tab bar halfway down.
    if (state.full !== false) {
      const height = Math.min(maxHeight, await page.evaluate(() => document.documentElement.scrollHeight))
      if (height > size.height) {
        await page.setViewportSize({ width: size.width, height })
        await page.waitForTimeout(250)
      }
    }
    await page.screenshot({ path: `${out}${file}`, animations: 'disabled' })
  } catch (e) {
    errors.push(`audit: ${String(e.message ?? e).split('\n')[0]}`)
    try {
      await page.screenshot({ path: `${out}${file}` })
    } catch {
      /* the page is gone; the error says so */
    }
  }
  await ctx.close()
  return { key: `${sizeName}-${scheme}`, file, misses: [...misses], errors }
}

if (!stateFilter && !onlyAreas) rmSync(out, { recursive: true, force: true })
mkdirSync(out, { recursive: true })

const vite = spawn('bun', ['run', 'dev', '--', '--port', String(port), '--strictPort', '--host', '127.0.0.1'], {
  cwd: spa,
  env: { ...process.env, TRACON_FIXTURES: '1' },
  stdio: 'ignore',
})
process.on('exit', () => vite.kill())
for (let i = 0; ; i++) {
  try {
    await fetch(`${base}/api/node`)
    break
  } catch {
    if (i > 80) throw new Error('vite did not come up')
    await new Promise((r) => setTimeout(r, 250))
  }
}

const executablePath = process.env.TRACON_CHROMIUM
const browser = await chromium.launch(executablePath ? { executablePath } : {})

const jobs = []
for (const s of states) {
  for (const size of sizeNames.filter((n) => !s.sizes || s.sizes.includes(n))) {
    for (const scheme of schemes) jobs.push({ s, size, scheme })
  }
}
const results = new Map(states.map((s) => [s.id, []]))
let next = 0
let done = 0
await Promise.all(
  Array.from({ length: workers }, async () => {
    while (next < jobs.length) {
      const job = jobs[next++]
      let r = await shoot(browser, job.s, job.size, job.scheme)
      // Under load a page can miss its timeout; one more try settles most.
      if (r.errors.some((e) => e.startsWith('audit:'))) r = await shoot(browser, job.s, job.size, job.scheme)
      results.get(job.s.id).push(r)
      done++
      const flag = r.misses.length || r.errors.length ? `  (${r.misses.length} missing, ${r.errors.length} errors)` : ''
      console.log(`[${done}/${jobs.length}] ${r.file}${flag}`)
    }
  }),
)
await browser.close()
vite.kill()

const manifest = states.map((s) => {
  const rs = results.get(s.id)
  return {
    id: s.id,
    area: s.area,
    route: s.route,
    title: s.title,
    note: s.note ?? '',
    since: s.since ?? '',
    shots: Object.fromEntries(rs.map((r) => [r.key, r.file])),
    misses: [...new Set(rs.flatMap((r) => r.misses))].sort(),
    errors: [...new Set(rs.flatMap((r) => r.errors))],
  }
})
const manifestFile = onlyAreas ? `manifest-${onlyAreas.join('-')}.json` : 'manifest.json'
// A run narrowed by --state updates its states in place and keeps the rest.
let merged = manifest
if (stateFilter) {
  try {
    const prior = JSON.parse(readFileSync(`${out}${manifestFile}`, 'utf8'))
    const fresh = new Map(manifest.map((m) => [m.id, m]))
    merged = [...prior.map((m) => fresh.get(m.id) ?? m), ...manifest.filter((m) => !prior.some((p) => p.id === m.id))]
  } catch {
    /* no earlier manifest: this run is the whole of it */
  }
}
writeFileSync(`${out}${manifestFile}`, JSON.stringify(merged, null, 2))
console.log(`${out}${manifestFile}`)
process.exit(0)
