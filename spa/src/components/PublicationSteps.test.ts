// PublicationSteps in a real Chromium at a phone's width, as the review
// page's decision area mounts it: the steps of a publication attempt from a
// `publication` frame, and from the record a review was loaded with while an
// attempt runs. Skipped when there is no browser to drive; set
// TRACON_CHROMIUM to point at one. In CI a missing browser fails instead, so
// a runner image without Chrome can't turn these into quiet skips.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

import type { PublicationProgress, PublicationView } from '../lib/types'

const here = fileURLToPath(new URL('.', import.meta.url))

const harness = `
  <script>
    import PublicationSteps from ${JSON.stringify(`${here}PublicationSteps.svelte`)}
    const { live, loaded } = window.steps
  </script>
  <PublicationSteps reviewId="r1" {live} {loaded} />
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
if (!browser && process.env.CI) throw new Error('PublicationSteps.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

const PLAN = ['verifying', 'inspecting_forge', 'pushing', 'confirming_push', 'opening_change', 'published'] as const

const frame = (over: Partial<PublicationProgress> = {}): PublicationProgress => ({
  id: 'p1',
  review_id: 'r1',
  node_id: 'n1',
  provider: 'github',
  branch: 'feat/a-branch-name-long-enough-to-wrap-on-a-phone-screen',
  outcome: 'in_progress',
  step: 'pushing',
  step_started_ms: Date.now(),
  steps: [...PLAN],
  reason: null,
  url: null,
  ...over,
})

const view = (over: Partial<PublicationView> = {}): PublicationView => ({
  id: 'p1',
  outcome: 'in_progress',
  note: null,
  url: null,
  attempts: 1,
  updated_ms: Date.now(),
  provider: 'github',
  project: 'owner/name',
  branch: 'feat/x',
  remedy: '',
  recoverable: false,
  refusal: null,
  step: 'confirming_push',
  step_started_ms: Date.now() - 5000,
  steps: [...PLAN],
  ...over,
})

async function open(steps: { live: PublicationProgress | null; loaded: PublicationView | null }): Promise<Page> {
  const page = await browser!.newPage({ viewport: { width: 390, height: 844 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8')
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${css}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div>` +
    `<script>window.steps = ${JSON.stringify(steps)}</script><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }))
  await page.goto('http://tracon.test/')
  await page.waitForSelector('ol.pub-steps', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return page
}

const states = (page: Page) =>
  page.locator('ol.pub-steps li').evaluateAll((lis) => lis.map((li) => [li.dataset.step, li.className.split(' ')[0]]))

test.skipIf(!browser)('a publication frame renders its planned steps with the current one running', async () => {
  const page = await open({ live: frame(), loaded: null })
  expect(await states(page)).toEqual([
    ['verifying', 'done'],
    ['inspecting_forge', 'done'],
    ['pushing', 'running'],
    ['confirming_push', 'pending'],
    ['opening_change', 'pending'],
    ['published', 'pending'],
  ])
  expect(await page.locator('li.running .spin').count()).toBe(1)
  expect(await page.getByText('Opening the pull request').isVisible()).toBe(true)
  // A long branch wraps rather than running past a phone's width.
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0)
  await page.close()
})

test.skipIf(!browser)('a review loaded mid-attempt shows the step its record is on, and how long it has taken', async () => {
  const page = await open({ live: null, loaded: view() })
  const running = page.locator('li.running')
  expect(await running.getAttribute('data-step')).toBe('confirming_push')
  expect(await running.locator('.elapsed').textContent()).toMatch(/^\d+s$/)
  await page.close()
})

test.skipIf(!browser)('a failure is marked on the step that failed, with its reason beside it', async () => {
  const page = await open({
    live: frame({ outcome: 'failed', step: 'opening_change', reason: 'HTTP 422: needs a reviewer' }),
    loaded: null,
  })
  const failed = page.locator('li.failed')
  expect(await failed.getAttribute('data-step')).toBe('opening_change')
  expect(await failed.locator('.reason').textContent()).toBe('HTTP 422: needs a reviewer')
  await page.close()
})

test.skipIf(!browser)('published, the last step links the change', async () => {
  const page = await open({
    live: frame({ outcome: 'published', step: 'published', url: 'https://github.test/pull/1234' }),
    loaded: null,
  })
  const link = page.locator('li[data-step="published"] a')
  expect(await link.textContent()).toBe('Published: #1234')
  expect(await link.getAttribute('href')).toBe('https://github.test/pull/1234')
  await page.close()
})
