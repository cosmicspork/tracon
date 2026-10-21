// CommitMessage in a real Chromium at a phone's width: the box wraps a long
// line instead of overflowing, numbers only the real lines, and hands back
// exactly what was typed. Skipped when there is no browser to drive; set
// TRACON_CHROMIUM to point at one. In CI a missing browser fails instead, so
// a runner image without Chrome can't turn these into quiet skips.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

const here = fileURLToPath(new URL('.', import.meta.url))

// The component mounted on its own, as the review screen binds it, with its
// value readable from the page. The harness is a virtual module, so its
// imports are resolved here, svelte's to its client build.
const harness = `
  <script>
    import CommitMessage from ${JSON.stringify(`${here}CommitMessage.svelte`)}
    let value = $state('')
    window.shipped = () => value
  </script>
  <CommitMessage label="commit message" bind:value />
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
if (!browser && process.env.CI) throw new Error('CommitMessage.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

async function open(width: number): Promise<Page> {
  const page = await browser!.newPage({ viewport: { width, height: 812 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8')
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  await page.setContent(
    `<!doctype html><meta name="viewport" content="width=device-width"><style>${css}</style>` +
      `<body style="margin:0;padding:0 16px"><div id="app"></div><script type="module">${js}</script></body>`,
  )
  await page.waitForSelector('textarea', { timeout: 3000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return page
}

const long = Array.from({ length: 50 }, (_, i) => `word${i % 10}`).join(' ').padEnd(300, 'x')
const typed = `fix(review): wrap the box\n\n${long}\nSigned-off-by: someone`

test.skipIf(!browser)('a 300-character line wraps inside the box at a phone width, with no sideways scroll', async () => {
  const page = await open(375)
  await page.locator('textarea').fill(typed)
  await page.waitForTimeout(50)
  const m = await page.evaluate(() => {
    const ta = document.querySelector('textarea')!
    return {
      page: document.documentElement.scrollWidth - window.innerWidth,
      box: ta.scrollWidth - ta.clientWidth,
      height: ta.clientHeight,
      line: parseFloat(getComputedStyle(ta).lineHeight),
    }
  })
  expect(m.page).toBeLessThanOrEqual(0)
  expect(m.box).toBeLessThanOrEqual(0)
  // Four real lines, but the long one takes several rows: autogrow sized for them.
  expect(m.height).toBeGreaterThan(6 * m.line)
  await page.close()
})

test.skipIf(!browser)('the gutter numbers each real line once, skipping its wrapped rows', async () => {
  const page = await open(375)
  await page.locator('textarea').fill(typed)
  await page.waitForTimeout(50)
  const gutter = await page.evaluate(() =>
    [...document.querySelectorAll<HTMLElement>('.gutter span')].map((s) => ({ n: s.textContent, top: s.offsetTop })),
  )
  expect(gutter.map((g) => g.n)).toEqual(['1', '2', '3', '4'])
  const [line, top] = await page.evaluate(() => {
    const style = getComputedStyle(document.querySelector('textarea')!)
    return [parseFloat(style.lineHeight), parseFloat(style.paddingTop)]
  })
  // Line 1 is numbered level with the textarea's first row.
  expect(gutter[0].top).toBeCloseTo(top, 0)
  // Line 3 wraps, so line 4 starts several rows below it, not one.
  expect(gutter[3].top - gutter[2].top).toBeGreaterThan(2 * line)
  expect(gutter[1].top - gutter[0].top).toBeCloseTo(line, 0)
  await page.close()
})

test.skipIf(!browser)('what ships is exactly what was typed, with no break added where it wrapped', async () => {
  const page = await open(375)
  await page.locator('textarea').pressSequentially(typed)
  expect(await page.evaluate(() => (window as unknown as { shipped: () => string }).shipped())).toBe(typed)
  await page.close()
})

test.skipIf(!browser)('the column 72 guide shows only when the box is that wide, and a long subject is flagged', async () => {
  const narrow = await open(375)
  await narrow.locator('textarea').fill(`${'s'.repeat(80)}\n\nbody`)
  await narrow.waitForTimeout(50)
  expect(await narrow.evaluate(() => document.querySelector('.cm')!.classList.contains('wide'))).toBe(false)
  expect(await narrow.locator('.hint').textContent()).toContain('80 characters')
  expect(await narrow.locator('.gutter span.long').textContent()).toBe('1')
  // A screen reader hears the note with the box, not only a sighted reader.
  const hint = await narrow.locator('.hint').getAttribute('id')
  expect(hint).toBeTruthy()
  expect(await narrow.locator('textarea').getAttribute('aria-describedby')).toBe(hint)
  await narrow.close()

  const wide = await open(1280)
  await wide.locator('textarea').fill('fix: short subject')
  await wide.waitForTimeout(50)
  expect(await wide.evaluate(() => document.querySelector('.cm')!.classList.contains('wide'))).toBe(true)
  expect(await wide.locator('.hint').count()).toBe(0)
  expect(await wide.locator('textarea').getAttribute('aria-describedby')).toBeNull()
  await wide.close()
})

test.skipIf(!browser)('a message taller than the box adds no scroll space under the page', async () => {
  const page = await open(375)
  await page.locator('textarea').fill(Array.from({ length: 200 }, (_, i) => `line ${i}`).join('\n'))
  await page.waitForTimeout(50)
  const m = await page.evaluate(() => ({ height: document.documentElement.scrollHeight, view: window.innerHeight }))
  expect(m.height).toBeLessThanOrEqual(m.view)
  await page.close()
})
