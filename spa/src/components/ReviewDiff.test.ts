// The review diff in a real Chromium, on CodeMirror's merge view: the mode
// the width picks, side by side aligned and scrolling as one, the choice
// remembered, and a switch that keeps the open files and the row in view.
// Skipped when there is no browser to drive; set TRACON_CHROMIUM to point at
// one. In CI a missing browser fails instead, so a runner image without
// Chrome can't turn these into quiet skips.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

const here = fileURLToPath(new URL('.', import.meta.url))

// The diff mounted as the review screen mounts it, below a reason being
// written, which a switch of mode must leave alone.
const harness = `
  <script>
    import ReviewDiff from ${JSON.stringify(`${here}ReviewDiff.svelte`)}
    const { diff, collapsed } = window.reviewDiff
  </script>
  <textarea id="reason"></textarea>
  <ReviewDiff {diff} {collapsed} />
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
if (!browser && process.env.CI) throw new Error('ReviewDiff.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(30000)

// Three files: a short one, a long one changed throughout (so the page
// scrolls well past its start), and a new one.
const long = Array.from({ length: 120 }, (_, i) => `    let value_${i} = compute(${i});`)
const DIFF =
  `diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1,3 +1,3 @@\n # orbit\n-A gateway.\n+A gateway for brokered calls.\n More below.\n` +
  `diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,120 +1,150 @@\n` +
  long
    .map((l, i) =>
      i % 8 === 3
        ? `-${l}\n+${l.replace('compute', 'measure')}\n+    // and a note on value ${i}\n+    // over two lines`
        : i % 8 === 6
          ? `-${l}`
          : ` ${l}`,
    )
    .join('\n') +
  '\n' +
  `diff --git a/src/new.rs b/src/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,2 @@\n+pub fn new() {}\n+pub fn old() {}\n`

type Opts = { width?: number; collapsed?: boolean; init?: string }

async function open({ width = 1280, collapsed = false, init = '' }: Opts = {}, page?: Page): Promise<Page> {
  page ??= await browser!.newPage({ viewport: { width, height: 800 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8')
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${css}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div>` +
    `<script>${init};window.reviewDiff = ${JSON.stringify({ diff: DIFF, collapsed })}</script><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => route.fulfill({ contentType: 'text/html', body: html }))
  await page.goto('http://tracon.test/')
  await page.waitForSelector('.review-diff', { timeout: 5000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return page
}

/** Wait for the merge view of a file to be laid out. */
async function ready(page: Page, path: string) {
  await page.waitForSelector(`details[data-path="${path}"] .cm-editor .cm-line`, { timeout: 5000 })
  await page.waitForTimeout(100)
}

const mode = (page: Page) =>
  page.evaluate(() => (document.querySelector('[role=radio][aria-checked=true]') as HTMLElement | null)?.textContent?.trim())

/** The top on screen of the first row in a file's editor (or editors) reading `text`, per editor. */
const tops = (page: Page, path: string, text: string) =>
  page.evaluate(
    ([path, text]) =>
      [...document.querySelectorAll(`details[data-path="${path}"] .cm-editor`)].map((ed) => {
        const row = [...ed.querySelectorAll('.cm-line')].find((l) => l.textContent === text)
        return row ? Math.round(row.getBoundingClientRect().top) : null
      }),
    [path, text],
  )

test.skipIf(!browser)('a wide diff opens side by side, its rows aligned and scrolling as one', async () => {
  const page = await open()
  await ready(page, 'src/lib.rs')
  expect(await mode(page)).toBe('Side by side')
  expect(await page.locator('details[data-path="src/lib.rs"] .cm-mergeView').count()).toBe(1)

  // A context row well past several changes of differing size sits at the
  // same height on both sides.
  const row = `    let value_100 = compute(100);`
  await page.evaluate(() => window.scrollTo(0, 1800))
  await page.waitForTimeout(150)
  const [a, b] = await tops(page, 'src/lib.rs', row)
  expect(a).not.toBeNull()
  expect(a).toBe(b)

  // Scrolled further, they still move together.
  await page.evaluate(() => window.scrollBy(0, 120))
  await page.waitForTimeout(100)
  const [a2, b2] = await tops(page, 'src/lib.rs', row)
  expect(a2).toBe(b2)
  expect(a2).toBe(a! - 120)

  // An added row has a gap opposite it, not the next context row.
  const added = await tops(page, 'src/lib.rs', '    // over two lines')
  expect(added[0]).toBeNull()

  // Sideways, each side follows the other.
  const left = await page.evaluate(() => {
    const [a, b] = document.querySelectorAll('details[data-path="src/lib.rs"] .cm-scroller') as NodeListOf<HTMLElement>
    a.style.maxWidth = '120px' // make the side scroll sideways at this width
    b.style.maxWidth = '120px'
    a.scrollLeft = 40
    return new Promise<number>((r) => setTimeout(() => r(b.scrollLeft), 100))
  })
  expect(left).toBe(40)
  await page.close()
})

test.skipIf(!browser)('a narrow diff opens unified, its files folded on the phone', async () => {
  const page = await open({ width: 390, collapsed: true })
  expect(await mode(page)).toBe('Unified')
  expect(await page.locator('details[open]').count()).toBe(0)
  await page.locator('summary', { hasText: 'src/lib.rs' }).click()
  await ready(page, 'src/lib.rs')
  expect(await page.locator('.cm-mergeView').count()).toBe(0)
  expect(await page.locator('details[data-path="src/lib.rs"] .cm-deletedChunk').count()).toBeGreaterThan(0)
  // Nothing runs past a phone's width.
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0)
  await page.close()
})

test.skipIf(!browser)('the chosen mode is remembered for this viewer, whatever the width', async () => {
  const page = await open()
  await ready(page, 'src/lib.rs')
  await page.getByRole('radio', { name: 'Unified' }).click()
  expect(await page.evaluate(() => localStorage.getItem('tracon-diff-mode'))).toBe('unified')
  await open({}, page)
  await ready(page, 'src/lib.rs')
  expect(await mode(page)).toBe('Unified')
  expect(await page.locator('.cm-mergeView').count()).toBe(0)
  await page.close()

  // Chosen on the phone too: a narrow screen keeps side by side once picked.
  const phone = await open({ width: 390, init: "localStorage.setItem('tracon-diff-mode', 'split')" })
  expect(await mode(phone)).toBe('Side by side')
  await phone.close()
})

test.skipIf(!browser)('switching keeps the file, the row in view and the reason being written', async () => {
  const page = await open()
  await ready(page, 'src/lib.rs')
  await page.locator('#reason').fill('Rename compute to measure everywhere.')
  // Fold the first file: which files are open is part of where the reader is.
  await page.locator('summary', { hasText: 'README.md' }).click()

  // Scroll well into the long file and take an unchanged row a little below
  // the top of the window, then bring it to the top, a few pixels of it above.
  await page.evaluate(() => window.scrollTo(0, 1500))
  await page.waitForTimeout(150)
  const row = await page.evaluate(() => {
    const eds = document.querySelectorAll('details[data-path="src/lib.rs"] .cm-editor')
    const rows = [...eds[eds.length - 1].querySelectorAll('.cm-line')]
    return rows.find((l) => l.getBoundingClientRect().top > 60 && /compute\(\d+\)/.test(l.textContent ?? ''))!.textContent!
  })
  const at = async () => (await tops(page, 'src/lib.rs', row)).filter((t) => t !== null).pop()!
  await page.evaluate((y) => window.scrollBy(0, y + 4), await at())
  await page.waitForTimeout(150)
  const before = await at()
  expect(before).toBe(-4)

  for (const next of ['Unified', 'Side by side', 'Unified']) {
    await page.getByRole('radio', { name: next }).click()
    await page.waitForTimeout(300)
    expect(await mode(page)).toBe(next)
    expect(Math.abs((await at()) - before)).toBeLessThanOrEqual(2)
  }
  expect(await page.locator('details[data-path="README.md"]').getAttribute('open')).toBeNull()
  expect(await page.locator('details[data-path="src/lib.rs"]').getAttribute('open')).not.toBeNull()
  expect(await page.locator('#reason').inputValue()).toBe('Rename compute to measure everywhere.')
  await page.close()
})

test.skipIf(!browser)('storage that refuses still shows and switches the diff', async () => {
  const page = await open({
    init: "Object.defineProperty(window, 'localStorage', { get() { throw new Error('SecurityError') } })",
  })
  await ready(page, 'src/lib.rs')
  expect(await mode(page)).toBe('Side by side')
  await page.getByRole('radio', { name: 'Unified' }).click()
  expect(await mode(page)).toBe('Unified')
  await page.close()
})
