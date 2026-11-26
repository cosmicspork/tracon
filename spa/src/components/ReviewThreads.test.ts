// ReviewThreads in a real Chromium: unused, the section is one line that
// opens the composer; with threads it folds to a count, open while any asks
// for something. Skipped when there is no browser to drive; set
// TRACON_CHROMIUM to point at one. In CI a missing browser fails instead.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

import type { ReviewThread, ThreadState } from '../lib/reviewThreads'
import { appCss } from '../lib/app-css'

const here = fileURLToPath(new URL('.', import.meta.url))

// The component mounted on its own, bound as the review screen binds it; the
// page's `pick` stands in for a line number picked in the diff gutter.
const harness = `
  <script>
    import ReviewThreads from ${JSON.stringify(`${here}ReviewThreads.svelte`)}
    const given = window.given
    let anchor = $state({ kind: 'general' })
    let composing = $state(false)
    let threads = $state(given.threads)
    window.pick = () => {
      anchor = { kind: 'line', path: 'src/a.rs', side: 'new', start_line: 3, end_line: 3 }
      composing = true
    }
  </script>
  <ReviewThreads
    reviewId="r"
    revisionId="rev1"
    revisionNumber={1}
    paths={['src/a.rs']}
    {threads}
    bind:anchor
    bind:composing
    blocked={null}
    onthreads={(next) => (threads = next)}
  />
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
if (!browser && process.env.CI) throw new Error('ReviewThreads.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

function thread(id: string, state: ThreadState): ReviewThread {
  return {
    id,
    kind: 'general',
    revision_id: 'rev1',
    revision: 1,
    state,
    outdated: state === 'outdated',
    comments: [{ author: 'operator', body: `thread ${id}`, revision: 1, at_ms: 0 }],
    created_ms: 0,
    updated_ms: 0,
  }
}

async function open(threads: ReviewThread[]): Promise<Page> {
  const page = await browser!.newPage({ viewport: { width: 390, height: 844 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${appCss()}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div>` +
    `<script>window.given = ${JSON.stringify({ threads })}</script><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }))
  await page.goto('http://tracon.test/')
  await page.waitForSelector('.h4', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return page
}

test.skipIf(!browser)('with no threads the section is one line, and + Thread opens a general composer', async () => {
  const page = await open([])
  const head = page.locator('.h4')
  expect((await head.innerText()).replace(/\s+/g, ' ').trim()).toBe('Threads · none + Thread')
  expect(await page.locator('textarea').count()).toBe(0)
  // The composer's explanation is behind the heading's help, not under it.
  expect(await page.getByText('Pick a line number in the diff').count()).toBe(0)
  await head.getByRole('button', { name: '+ Thread' }).click()
  expect(await page.locator('textarea[aria-label="new thread"]').isVisible()).toBe(true)
  expect(await page.locator('select').first().inputValue()).toBe('general')
  await page.getByRole('button', { name: 'Cancel' }).click()
  expect(await page.locator('textarea').count()).toBe(0)
  await head.getByRole('button', { name: 'About threads' }).click()
  expect(await page.getByText('Pick a line number in the diff', { exact: false }).isVisible()).toBe(true)
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0)
  await page.close()
})

test.skipIf(!browser)('a line picked in the diff opens the composer at that line', async () => {
  const page = await open([])
  await page.evaluate(() => (window as unknown as { pick: () => void }).pick())
  expect(await page.locator('textarea[aria-label="new thread"]').isVisible()).toBe(true)
  expect(await page.locator('select').first().inputValue()).toBe('line')
  await page.close()
})

test.skipIf(!browser)('threads all resolved or outdated start folded to their count', async () => {
  const page = await open([thread('a', 'resolved'), thread('b', 'outdated')])
  const fold = page.locator('.h4 button.fold')
  expect((await fold.innerText()).replace(/\s+/g, ' ')).toContain('Threads · 1 open · 1 resolved')
  expect(await fold.getAttribute('aria-expanded')).toBe('false')
  expect(await page.locator('article.thread').count()).toBe(0)
  await fold.click()
  expect(await page.getByText('thread b').isVisible()).toBe(true)
  await page.close()
})

test.skipIf(!browser)('a thread unresolved or addressed opens the section', async () => {
  for (const state of ['unresolved', 'addressed'] as const) {
    const page = await open([thread('a', 'resolved'), thread('b', state)])
    const fold = page.locator('.h4 button.fold')
    expect(await fold.getAttribute('aria-expanded')).toBe('true')
    expect(await page.getByText('thread b').isVisible()).toBe(true)
    await fold.click()
    expect(await page.locator('article.thread').count()).toBe(0)
    await page.close()
  }
})
