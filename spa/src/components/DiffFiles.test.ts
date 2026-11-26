// DiffFiles in a real Chromium: the frame the review's diff is read and
// edited in, with the filter, folding and stepping both share. Skipped when
// there is no browser to drive; set TRACON_CHROMIUM to point at one. In CI a
// missing browser fails instead.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

import { appCss } from '../lib/app-css'

const here = fileURLToPath(new URL('.', import.meta.url))

// The frame mounted on its own, as Edit the diff mounts it: a body and a chip
// per file from the caller.
const harness = `
  <script>
    import DiffFiles from ${JSON.stringify(`${here}DiffFiles.svelte`)}
    const { items, opened, edited } = window.given
  </script>
  <DiffFiles {items} startOpen={() => opened}>
    {#snippet chips(path)}{#if edited.includes(path)}<span class="chip warn">edited</span>{/if}{/snippet}
    {#snippet body(path)}<pre class="content" style="height: 900px">body of {path}</pre>{/snippet}
  </DiffFiles>
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
if (!browser && process.env.CI) throw new Error('DiffFiles.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

const ITEMS = [
  { path: 'src/a.rs', added: 3, removed: 1, counted: true },
  { path: 'src/b.rs', added: 2, removed: 0, counted: true },
  { path: 'docs/c.md', added: 1, removed: 1, counted: true },
  { path: 'logo.png', added: 0, removed: 0, counted: false },
]

async function open(opened: string[] = [], edited: string[] = []): Promise<Page> {
  const page = await browser!.newPage({ viewport: { width: 1280, height: 800 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${appCss()}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div>` +
    `<script>window.given = ${JSON.stringify({ items: ITEMS, opened, edited })}</script><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }))
  await page.goto('http://tracon.test/')
  await page.waitForSelector('.diff-files', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return page
}

test.skipIf(!browser)('each file is a header with its counts and the caller\'s chips, open as the caller says', async () => {
  const page = await open(['src/b.rs'], ['src/b.rs'])
  expect(await page.locator('details.file').count()).toBe(4)
  expect(await page.locator('details.file[open]').evaluateAll((els) => els.map((e) => (e as HTMLElement).dataset.path))).toEqual(['src/b.rs'])
  expect(await page.locator('details[data-path="src/a.rs"] .stat').textContent()).toBe('+3 −1')
  expect(await page.locator('details[data-path="logo.png"] .stat').count()).toBe(0)
  expect(await page.locator('details[data-path="src/b.rs"] summary .chip.warn').textContent()).toBe('edited')
  expect(await page.locator('details[data-path="src/a.rs"] summary .chip').count()).toBe(0)
  expect(await page.getByText('body of src/b.rs').isVisible()).toBe(true)
  expect(await page.getByText('body of src/a.rs').count()).toBe(0)
  expect(await page.locator('.bar .total').textContent()).toBe('+6 −2')
  // No viewed marks without a reader's progress to keep.
  expect(await page.locator('.bar .progress').count()).toBe(0)
  await page.close()
})

test.skipIf(!browser)('the filter narrows the files, and Fold all / Open all act on those it shows', async () => {
  const page = await open()
  await page.locator('.filter').fill('src/')
  expect(await page.locator('.bar .count').textContent()).toBe('2 of 4')
  await page.getByRole('button', { name: 'Open all' }).click()
  expect(await page.locator('details.file[open]').evaluateAll((els) => els.map((e) => (e as HTMLElement).dataset.path))).toEqual(['src/a.rs', 'src/b.rs'])
  await page.locator('.filter').fill('nothing')
  expect(await page.locator('.diff-files > .empty').textContent()).toContain('No file path contains')
  await page.locator('.filter').fill('')
  await page.getByRole('button', { name: 'Fold all' }).click()
  expect(await page.locator('details.file[open]').count()).toBe(0)
  await page.locator('details[data-path="docs/c.md"] summary').click()
  await page.getByText('body of docs/c.md').waitFor({ timeout: 2000 })
  await page.close()
})

test.skipIf(!browser)('past the open files, n opens the next folded one and brings it to the top', async () => {
  const page = await open(['src/a.rs'])
  const top = (path: string) => page.locator(`details[data-path="${path}"]`).evaluate((e) => e.getBoundingClientRect().top)
  // Past the last open change, the next folded file opens and comes to the top.
  await page.keyboard.press('n')
  await page.waitForTimeout(300)
  expect(await page.locator('details[data-path="src/b.rs"]').getAttribute('open')).not.toBeNull()
  expect(Math.abs(await top('src/b.rs'))).toBeLessThan(120)
  await page.close()
})
