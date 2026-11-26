// The review header's identity in a real Chromium: the name and the id chip
// each open the full id beside them, and Copy takes it, two clicks from the
// page. Skipped when there is no browser to drive; set TRACON_CHROMIUM to
// point at one. In CI a missing browser fails instead.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

import { appCss } from '../lib/app-css'

const here = fileURLToPath(new URL('.', import.meta.url))

const harness = `
  <script>
    import ReviewId from ${JSON.stringify(`${here}ReviewId.svelte`)}
  </script>
  <p><ReviewId kind="review" id="01a21416-7458-77b0-a822-408450a14cc8" /></p>
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
if (!browser && process.env.CI) throw new Error('ReviewId.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

const ID = '01a21416-7458-77b0-a822-408450a14cc8'

async function open(): Promise<Page> {
  const page = await browser!.newPage({ viewport: { width: 390, height: 844 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  // What Copy writes, read back without a clipboard permission.
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: async (t: string) => void ((window as unknown as { copied: string }).copied = t) },
    })
  })
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${appCss()}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }))
  await page.goto('http://tracon.test/')
  await page.waitForSelector('.idchip', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return page
}

test.skipIf(!browser)('the id chip shows the first 8 hex digits and opens the full id, which Copy takes', async () => {
  const page = await open()
  const chip = page.locator('.idchip')
  expect(await chip.textContent()).toBe('01a21416')
  expect(await page.getByRole('dialog').count()).toBe(0)
  await chip.click()
  expect(await page.getByRole('dialog').locator('code').textContent()).toBe(ID)
  await page.getByRole('dialog').getByRole('button', { name: 'Copy' }).click()
  expect(await page.evaluate(() => (window as unknown as { copied?: string }).copied)).toBe(ID)
  expect(await page.getByRole('dialog').getByRole('button', { name: 'Copied' }).isVisible()).toBe(true)
  await page.close()
})

test.skipIf(!browser)('clicking the name opens it too, and a click outside or Escape closes it', async () => {
  const page = await open()
  await page.locator('[data-testid="name"] .n').click()
  expect(await page.getByRole('dialog').isVisible()).toBe(true)
  await page.mouse.click(380, 800)
  expect(await page.getByRole('dialog').count()).toBe(0)
  await page.locator('.idchip').click()
  await page.keyboard.press('Escape')
  expect(await page.getByRole('dialog').count()).toBe(0)
  await page.close()
})
