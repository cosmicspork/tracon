// Settings › Plugins in a real Chromium, against a node stubbed route by
// route: checking for an update (up to date, a card raised, a local
// archive refused) and rolling back after the in-page confirmation. Skipped
// when there is no browser to drive; set TRACON_CHROMIUM to point at one.
// In CI a missing browser fails instead.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page, type Route } from 'playwright-core'
import { compile } from 'svelte/compiler'

import type { PluginSummary } from '../../lib/types'

const here = fileURLToPath(new URL('.', import.meta.url))

const harness = `
  <script>
    import Plugins from ${JSON.stringify(`${here}Plugins.svelte`)}
  </script>
  <Plugins channels={['personal', 'orbit']} />
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
if (!browser && process.env.CI) throw new Error('Plugins.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

const RELEASED: PluginSummary = {
  name: 'sql-review',
  repo: 'example-org/sql-review',
  state: 'installed',
  active: { version: '0.3.1', tools: [] },
  previous: { version: '0.3.0', digest: 'aa' },
  pending: null,
  instances: [],
  tools: [],
}
const LOCAL: PluginSummary = {
  name: 'tickets',
  repo: 'local:tickets-1.2.0-x86_64-unknown-linux-musl.tar.gz',
  state: 'installed',
  active: { version: '1.2.0', tools: [] },
  previous: null,
  pending: null,
  instances: [],
  tools: [],
}

type Answer = { status?: number; body: unknown }
type Reply = Answer | (() => Answer | Promise<Answer>)
const fail = (status: number, message: string) => ({ status, body: { error: { code: status, message } } })

/** What each stubbed request was sent, by `METHOD path`. */
type Seen = Record<string, unknown[]>

async function open(routes: Record<string, Reply>): Promise<{ page: Page; seen: Seen }> {
  const page = await browser!.newPage({ viewport: { width: 390, height: 844 } })
  const errors: string[] = []
  const seen: Seen = {}
  page.on('pageerror', (e) => errors.push(String(e)))
  const css = readFileSync(new URL('../../app.css', import.meta.url), 'utf8')
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${css}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', async (route: Route) => {
    const url = new URL(route.request().url())
    if (!url.pathname.startsWith('/api/')) return route.fulfill({ contentType: 'text/html', body: html })
    const key = `${route.request().method()} ${url.pathname}`
    ;(seen[key] ??= []).push(route.request().postData() === null ? null : JSON.parse(route.request().postData()!))
    const reply = routes[key]
    if (!reply) return route.fulfill({ status: 404, contentType: 'application/json', body: JSON.stringify({ error: { message: `no stub for ${key}` } }) })
    const r = typeof reply === 'function' ? await reply() : reply
    return route.fulfill({ status: r.status ?? 200, contentType: 'application/json', body: JSON.stringify(r.body) })
  })
  await page.goto('http://tracon.test/')
  await page.waitForSelector('li[data-plugin]', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return { page, seen }
}

const row = (page: Page, name: string) => page.locator(`li[data-plugin="${name}"]`)

test.skipIf(!browser)('a plugin whose active build is the latest says it is up to date', async () => {
  const { page, seen } = await open({
    'GET /api/plugins': { body: { plugins: [RELEASED] } },
    'POST /api/plugins/sql-review/update': { body: { up_to_date: true, version: '0.3.1', digest: 'bb' } },
  })
  await page.getByLabel('Channel update cards go to').selectOption('orbit')
  await row(page, 'sql-review').getByRole('button', { name: 'Check for update' }).click()
  await row(page, 'sql-review').getByText('Up to date: 0.3.1 is the latest release.').waitFor()
  // The card would go to the channel being viewed.
  expect(seen['POST /api/plugins/sql-review/update']).toEqual([{ channel: 'orbit' }])
  await page.close()
})

test.skipIf(!browser)('a raised update card is linked, and the buttons wait on it', async () => {
  let raised = false
  let release: () => void = () => {}
  const held = new Promise<void>((resolve) => (release = resolve))
  const { page } = await open({
    'GET /api/plugins': () => ({
      body: {
        plugins: [
          raised
            ? { ...RELEASED, pending: { version: '0.4.0', digest: 'cc', approval_id: 'ap-1', approval_state: 'pending' } }
            : RELEASED,
        ],
      },
    }),
    'POST /api/plugins/sql-review/update': async () => {
      await held
      raised = true
      return {
        body: { name: 'sql-review', version: '0.4.0', from_version: '0.3.1', digest: 'cc', approval_id: 'ap-1', channel: 'personal', new: true },
      }
    },
  })
  const r = row(page, 'sql-review')
  await r.getByRole('button', { name: 'Check for update' }).click()
  // In flight: neither button can be pressed again.
  expect(await r.getByRole('button', { name: 'Checking…' }).isDisabled()).toBe(true)
  expect(await r.getByRole('button', { name: 'Roll back to 0.3.0' }).isDisabled()).toBe(true)
  release()
  const link = r.getByRole('link', { name: 'Open the card' })
  await link.waitFor()
  expect(await link.getAttribute('href')).toBe('/approvals/ap-1')
  expect(await r.getByText('An update card for 0.4.0 is waiting.').isVisible()).toBe(true)
  // While it waits, both stay disabled.
  expect(await r.getByRole('button', { name: 'Check for update' }).isDisabled()).toBe(true)
  expect(await r.getByRole('button', { name: 'Roll back to 0.3.0' }).isDisabled()).toBe(true)
  await page.close()
})

test.skipIf(!browser)("a local archive's plugin shows the node's refusal and no retry", async () => {
  const message = 'tickets was installed from a local archive; add the new archive again to update it'
  const { page } = await open({
    'GET /api/plugins': { body: { plugins: [RELEASED, LOCAL] } },
    'POST /api/plugins/tickets/update': fail(422, message),
  })
  const r = row(page, 'tickets')
  await r.getByRole('button', { name: 'Check for update' }).click()
  await r.getByText(message).waitFor()
  expect(await r.getByRole('button', { name: 'Check for update' }).count()).toBe(0)
  await page.close()
})

test.skipIf(!browser)('another failure shows the node\'s message and can be tried again', async () => {
  const { page } = await open({
    'GET /api/plugins': { body: { plugins: [RELEASED] } },
    'POST /api/plugins/sql-review/update': fail(422, "example-org/sql-review's latest release is plugin hoja, not sql-review"),
  })
  const r = row(page, 'sql-review')
  await r.getByRole('button', { name: 'Check for update' }).click()
  await r.getByText('latest release is plugin hoja').waitFor()
  expect(await r.getByRole('button', { name: 'Check for update' }).isEnabled()).toBe(true)
  await page.close()
})

test.skipIf(!browser)('a rollback asks in the page, naming both versions, then shows the swapped builds', async () => {
  let rolled = false
  const { page, seen } = await open({
    'GET /api/plugins': () => ({
      body: {
        plugins: [
          rolled
            ? { ...RELEASED, active: { version: '0.3.0', tools: [] }, previous: { version: '0.3.1', digest: 'bb' } }
            : RELEASED,
        ],
      },
    }),
    'POST /api/plugins/sql-review/rollback': () => {
      rolled = true
      return { body: { active: { version: '0.3.0', digest: 'aa' }, previous: { version: '0.3.1', digest: 'bb' } } }
    },
  })
  let dialogs = 0
  page.on('dialog', (d) => {
    dialogs++
    void d.dismiss()
  })
  const r = row(page, 'sql-review')
  await r.getByRole('button', { name: 'Roll back to 0.3.0' }).click()
  const confirm = r.getByRole('group', { name: 'Confirm the rollback' })
  expect(await confirm.getByText('Make 0.3.0 active again in place of 0.3.1?').isVisible()).toBe(true)
  // Cancelling sends nothing.
  await confirm.getByRole('button', { name: 'Cancel' }).click()
  expect(await confirm.count()).toBe(0)
  expect(seen['POST /api/plugins/sql-review/rollback']).toBeUndefined()
  await r.getByRole('button', { name: 'Roll back to 0.3.0' }).click()
  await r.getByRole('group', { name: 'Confirm the rollback' }).getByRole('button', { name: 'Roll back to 0.3.0' }).click()
  await r.getByText('Rolled back to 0.3.0; 0.3.1 is now the previous version.').waitFor()
  expect(seen['POST /api/plugins/sql-review/rollback']).toHaveLength(1)
  expect(await r.getByText('0.3.0 · example-org/sql-review').isVisible()).toBe(true)
  expect(await r.getByText('previous 0.3.1').isVisible()).toBe(true)
  expect(await r.getByRole('button', { name: 'Roll back to 0.3.1' }).isVisible()).toBe(true)
  expect(dialogs).toBe(0)
  // Nothing in the row runs past a phone's width.
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0)
  await page.close()
})

test.skipIf(!browser)('a plugin with no previous build offers no rollback', async () => {
  const { page } = await open({ 'GET /api/plugins': { body: { plugins: [LOCAL] } } })
  const r = row(page, 'tickets')
  expect(await r.getByRole('button', { name: /Roll back/ }).count()).toBe(0)
  expect(await r.getByText('no previous version').isVisible()).toBe(true)
  expect(await r.getByRole('button', { name: 'Check for update' }).isVisible()).toBe(true)
  await page.close()
})
