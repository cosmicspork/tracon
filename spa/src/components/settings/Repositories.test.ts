// Settings › Repository environments in a real Chromium, against a node
// stubbed route by route: a removal is staged (struck through, with Undo)
// until saved, a save bar holds the unsaved count, leaving with unsaved
// changes asks, and a save that meets a changed node.toml keeps the removal
// pending. Skipped when there is no browser to drive; set TRACON_CHROMIUM to
// point at one. In CI a missing browser fails instead.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page, type Route } from 'playwright-core'
import { compile } from 'svelte/compiler'

import { appCss } from '../../lib/app-css'

const here = fileURLToPath(new URL('.', import.meta.url))

const harness = `
  <script>
    import Repositories from ${JSON.stringify(`${here}Repositories.svelte`)}
    import { router } from ${JSON.stringify(`${here}../../lib/router.svelte.ts`)}
    router.start()
  </script>
  <a href="/elsewhere" id="away">away</a>
  <Repositories />
`
const entry = `
  import { mount } from 'svelte'
  import Harness from 'harness.svelte'
  mount(Harness, { target: document.getElementById('app') })
`
const svelteClient = fileURLToPath(new URL('src/index-client.js', Bun.pathToFileURL(Bun.resolveSync('svelte/package.json', here))))
const compiled = (source: string, filename: string) =>
  compile(source, { filename, generate: 'client', css: 'injected' }).js.code

let bundled: Promise<string> | undefined
const bundle = () => (bundled ??= build())

async function build(): Promise<string> {
  const out = await Bun.build({
    entrypoints: ['entry'],
    target: 'browser',
    plugins: [
      {
        name: 'svelte',
        setup(b) {
          b.onResolve({ filter: /^(entry|harness\.svelte)$/ }, ({ path }) => ({ path, namespace: 'harness' }))
          b.onResolve({ filter: /^svelte(\/|$)/ }, ({ path }) => ({ path: path === 'svelte' ? svelteClient : Bun.resolveSync(path, here) }))
          b.onLoad({ filter: /.*/, namespace: 'harness' }, ({ path }) => ({
            contents: path === 'entry' ? entry : compiled(harness, 'Harness.svelte'),
            loader: 'js',
          }))
          b.onLoad({ filter: /\.svelte\.ts$/ }, async ({ path }) => ({
            contents: (await import('svelte/compiler')).compileModule(
              new Bun.Transpiler({ loader: 'ts' }).transformSync(readFileSync(path, 'utf8')),
              { filename: path, generate: 'client' },
            ).js.code,
            loader: 'js',
          }))
          b.onLoad({ filter: /\.svelte$/ }, ({ path }) => ({ contents: compiled(readFileSync(path, 'utf8'), path), loader: 'js' }))
        },
      },
    ],
  })
  if (!out.success) throw new Error(out.logs.join('\n'))
  return out.outputs[0].text()
}

async function launch(): Promise<Browser | null> {
  const tries = [process.env.TRACON_CHROMIUM ? { executablePath: process.env.TRACON_CHROMIUM } : {}, { channel: 'chrome' }]
  for (const t of tries) {
    try {
      return await chromium.launch(t)
    } catch {
      /* the next place a browser might be */
    }
  }
  return null
}


const browser = await launch()
if (!browser && process.env.CI) throw new Error('Repositories.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)


const TABLE = [
  { path: 'github.com/o/kept', prepare: [], egress: [] },
  { path: 'github.com/o/gone', prepare: [], egress: [] },
]
const ENVIRONMENTS = { can_build: false, hash: 'h1', presets: [], entries: [] }

type Answer = { status?: number; body: unknown }
type Reply = Answer | (() => Answer | Promise<Answer>)

/** What each stubbed request was sent, by `METHOD path`. */
type Seen = Record<string, unknown[]>

async function open(routes: Record<string, Reply>): Promise<{ page: Page; seen: Seen }> {
  const page = await browser!.newPage({ viewport: { width: 390, height: 844 } })
  const errors: string[] = []
  const seen: Seen = {}
  page.on('pageerror', (e) => errors.push(String(e)))
  const css = appCss()
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${css}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div><script type="module">${js}</script></body>`
  const all: Record<string, Reply> = {
    'GET /api/repos/environments': { body: ENVIRONMENTS },
    'GET /api/repos/offers': { body: { offers: [] } },
    ...routes,
  }
  await page.route('http://tracon.test/**', async (route: Route) => {
    const url = new URL(route.request().url())
    if (!url.pathname.startsWith('/api/')) return route.fulfill({ contentType: 'text/html', body: html })
    const key = `${route.request().method()} ${url.pathname}`
    ;(seen[key] ??= []).push(route.request().postData() === null ? null : JSON.parse(route.request().postData()!))
    const reply = all[key]
    if (!reply) return route.fulfill({ status: 404, contentType: 'application/json', body: JSON.stringify({ error: { message: `no stub for ${key}` } }) })
    const r = typeof reply === 'function' ? await reply() : reply
    return route.fulfill({ status: r.status ?? 200, contentType: 'application/json', body: JSON.stringify(r.body) })
  })
  await page.goto('http://tracon.test/settings#repositories')
  await page.waitForSelector('details.entry', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return { page, seen }
}

const config = (hash: string, repo = TABLE) => ({ body: { document: { repo }, hash } })
const removeGone = (page: Page) =>
  page.locator('details.entry', { hasText: 'github.com/o/gone' }).getByRole('button', { name: 'Remove this entry' }).click()

test.skipIf(!browser)('a removal stays in view, struck through, until saved, and Undo puts it back', async () => {
  const { page } = await open({ 'GET /api/config': config('h1') })
  expect(await page.getByRole('region', { name: 'Unsaved repository changes' }).count()).toBe(0)
  await removeGone(page)
  const removed = page.locator('[data-removed="github.com/o/gone"]')
  expect(await removed.locator('s').innerText()).toBe('github.com/o/gone')
  expect(await removed.innerText()).toContain('Will be removed when you save')
  const bar = page.getByRole('region', { name: 'Unsaved repository changes' })
  expect(await bar.innerText()).toContain('1 unsaved change')
  await removed.getByRole('button', { name: 'Undo' }).click()
  expect(await removed.count()).toBe(0)
  expect(await bar.count()).toBe(0)
  expect(await page.locator('details.entry').allInnerTexts()).toHaveLength(2)
  await page.close()
})

test.skipIf(!browser)('leaving with unsaved changes asks first', async () => {
  const { page } = await open({ 'GET /api/config': config('h1') })
  await removeGone(page)
  const asked: string[] = []
  page.on('dialog', (dialog) => {
    asked.push(dialog.message())
    void dialog.dismiss()
  })
  await page.click('#away')
  expect(asked).toEqual(['Leave without saving your repository changes?'])
  expect(new URL(page.url()).pathname).toBe('/settings')
  expect(await page.locator('[data-removed="github.com/o/gone"]').count()).toBe(1)
  await page.close()
})

test.skipIf(!browser)('a save that meets a changed node.toml keeps the removal pending and says so', async () => {
  let reads = 0
  const moved = [...TABLE, { path: 'github.com/o/new', prepare: [], egress: [] }]
  const { page, seen } = await open({
    'GET /api/config': () => (reads++ === 0 ? config('h1') : config('h2', moved)),
    'PATCH /api/config': { status: 409, body: { error: { code: 409, message: 'node.toml changed', hash: 'h2' } } },
  })
  await removeGone(page)
  await page.getByRole('button', { name: 'Save repositories' }).click()
  await page.getByText('Changed on the node while you were editing.').waitFor()
  expect(seen['PATCH /api/config']).toHaveLength(1)
  const bar = page.getByRole('region', { name: 'Unsaved repository changes' })
  expect(await bar.innerText()).toContain('Not saved: node.toml changed; review and save')
  expect(await page.locator('[data-removed="github.com/o/gone"]').count()).toBe(1)
  expect(await page.evaluate(() => document.activeElement?.getAttribute('role'))).toBe('alert')
  await page.close()
})
