// ShownWork in a real Chromium: on a review, an item shown at an earlier
// candidate folds to its header so the current ones are read first, unless the
// address names it. Skipped when there is no browser to drive; set
// TRACON_CHROMIUM to point at one. In CI a missing browser fails instead, so
// a runner image without Chrome can't turn these into quiet skips.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

import type { ShownWork } from '../lib/types'

const here = fileURLToPath(new URL('.', import.meta.url))

// The component mounted on its own, as the review screen mounts it, with the
// items handed in from the page.
const harness = `
  <script>
    import ShownWork from ${JSON.stringify(`${here}ShownWork.svelte`)}
    const { items, onReview } = window.shown
  </script>
  <ShownWork {items} {onReview} />
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
if (!browser && process.env.CI) throw new Error('ShownWork.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

function item(id: string, stale: boolean, markdown: string): ShownWork {
  return {
    id,
    channel: 'c',
    session_id: 's',
    lane: null,
    review_id: 'r',
    head_sha: stale ? 'aaaaaaaaaaaaaaaa' : 'bbbbbbbbbbbbbbbb',
    title: 'Note from the submitter',
    format: 'markdown',
    markdown,
    document_id: null,
    document_slug: null,
    document_hash: null,
    files: [],
    created_ms: Date.now(),
    stale,
    stale_reason: stale ? 'shown at an earlier candidate' : null,
    checks: [],
  }
}

const items = [item('old', true, 'the earlier note'), item('new', false, 'the current note')]

// The page is served from an address of its own, so the hash it opens at is
// the router's, as it is when a link lands on the review.
async function open(hash = '', onReview = true): Promise<Page> {
  const page = await browser!.newPage({ viewport: { width: 390, height: 844 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8')
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${css}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div>` +
    `<script>window.shown = ${JSON.stringify({ items, onReview })}</script><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }))
  await page.goto(`http://tracon.test/${hash}`)
  await page.waitForSelector('article', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return page
}

test.skipIf(!browser)('a stale item on a review shows only its header and the current one shows in full', async () => {
  const page = await open()
  const stale = page.locator('#shown-old')
  expect(await stale.getByText('Note from the submitter').isVisible()).toBe(true)
  expect(await stale.locator('.chip', { hasText: 'stale' }).isVisible()).toBe(true)
  expect(await stale.getByText('the earlier note').isVisible()).toBe(false)
  expect(await page.locator('#shown-new').getByText('the current note').isVisible()).toBe(true)
  // Its header opens it.
  await stale.locator('summary').click()
  expect(await stale.getByText('the earlier note').isVisible()).toBe(true)
  // Nothing in the fold runs past a phone's width.
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0)
  await page.close()
})

test.skipIf(!browser)('a stale item the address names lands open', async () => {
  const page = await open('#shown-old')
  expect(await page.locator('#shown-old').getByText('the earlier note').isVisible()).toBe(true)
  await page.close()
})

test.skipIf(!browser)('on a session nothing folds, as a session has no candidate to be stale against', async () => {
  const page = await open('', false)
  expect(await page.locator('details.fold').count()).toBe(0)
  expect(await page.getByText('the earlier note').isVisible()).toBe(true)
  await page.close()
})
