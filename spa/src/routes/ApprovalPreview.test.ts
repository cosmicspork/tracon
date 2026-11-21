// The approval page in a real Chromium: a plugin preview's wide table keeps
// each header and cell on one line and scrolls sideways, rather than
// squeezing every column to a letter (#1037). Skipped when there is no
// browser to drive; set TRACON_CHROMIUM to point at one. In CI a missing
// browser fails instead, so a runner image without Chrome can't turn these
// into quiet skips.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser } from 'playwright-core'
import { compile } from 'svelte/compiler'

import type { ApprovalDetails } from '../lib/approval'

const here = fileURLToPath(new URL('.', import.meta.url))

const harness = `
  <script>
    import Approval from ${JSON.stringify(`${here}Approval.svelte`)}
  </script>
  <Approval id="a1" />
`
const entry = `
  import { mount } from 'svelte'
  import Harness from 'harness.svelte'
  mount(Harness, { target: document.getElementById('app') })
`
const svelteClient = fileURLToPath(new URL('src/index-client.js', Bun.pathToFileURL(Bun.resolveSync('svelte/package.json', here))))
const compiled = (source: string, filename: string) =>
  compile(source, { filename, generate: 'client', css: 'injected' }).js.code

async function bundle(): Promise<string> {
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
if (!browser && process.env.CI) throw new Error('ApprovalPreview.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

// sql-review's "Rows now": one row of a table with thirty columns.
const columns = Array.from({ length: 30 }, (_, i) => (i === 0 ? 'FIRST_NAME' : `COLUMN_${i}`))
const table = [
  `| ${columns.join(' | ')} |`,
  `| ${columns.map(() => '---').join(' | ')} |`,
  `| ${columns.map((_, i) => (i === 0 ? 'Ada' : `value ${i}`)).join(' | ')} |`,
].join('\n')

const now = Date.now()
const details: ApprovalDetails = {
  approval: {
    id: 'a1',
    channel: 'c',
    session_id: 's',
    node_id: 'n',
    lane: null,
    tool: 'sql-review__write',
    arguments: '{"sql":"UPDATE people SET first_name = \'Ada\'"}',
    request_key: 'k',
    title: 'sql-review__write',
    state: 'pending',
    answer_option_id: null,
    edited_arguments: null,
    result: null,
    reason: null,
    operator_note: null,
    claimed_ms: null,
    created_ms: now,
    decided_ms: null,
    finished_ms: null,
    expires_ms: now + 3_600_000,
  },
  arguments: { sql: "UPDATE people SET first_name = 'Ada'" },
  edited_arguments: null,
  result: null,
  input_schema: null,
  format: 'markdown',
  prose_fields: [],
  locked_fields: [],
  lane: null,
  claimed_before_ms: null,
  options: [
    { option_id: 'allow_once', name: 'Allow', kind: 'allow_once' },
    { option_id: 'reject_once', name: 'Reject', kind: 'reject_once' },
  ],
  preview: {
    status: 'shown',
    previewable: true,
    summary: '1 row',
    body: `A short note above the rows.\n\n### Rows now\n\n${table}\n`,
    token_sha256: null,
  },
}

test.skipIf(!browser)("a preview's wide table keeps its headers on one line and scrolls sideways", async () => {
  const page = await browser!.newPage({ viewport: { width: 1280, height: 900 } })
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const css = readFileSync(new URL('../app.css', import.meta.url), 'utf8')
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${css}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => {
    const path = new URL(route.request().url()).pathname
    if (path === '/api/approvals/a1') return route.fulfill({ contentType: 'application/json', body: JSON.stringify(details) })
    if (path.startsWith('/api/')) return route.fulfill({ status: 204, body: '' })
    return route.fulfill({ contentType: 'text/html', body: html })
  })
  await page.goto('http://tracon.test/')
  await page.waitForSelector('.preview table', { timeout: 5000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  const m = await page.evaluate(() => {
    const article = document.querySelector('.preview .md') as HTMLElement
    const table = article.querySelector('table') as HTMLElement
    const th = table.querySelector('th') as HTMLElement
    const td = table.querySelector('td') as HTMLElement
    const p = article.querySelector('p') as HTMLElement
    const line = parseFloat(getComputedStyle(th).lineHeight) || parseFloat(getComputedStyle(th).fontSize) * 1.6
    return {
      thWhiteSpace: getComputedStyle(th).whiteSpace,
      tdWhiteSpace: getComputedStyle(td).whiteSpace,
      thHeight: th.getBoundingClientRect().height,
      line,
      scrollWidth: table.scrollWidth,
      clientWidth: table.clientWidth,
      articleMax: getComputedStyle(article).maxWidth,
      proseMax: getComputedStyle(p).maxWidth,
      pageOverflow: document.documentElement.scrollWidth - window.innerWidth,
    }
  })
  expect(m.thWhiteSpace).toBe('nowrap')
  expect(m.tdWhiteSpace).toBe('nowrap')
  // One line, padding aside: not FIRST_NAME a letter a line.
  expect(m.thHeight).toBeLessThan(m.line * 2)
  // The table scrolls inside itself instead, and the page does not.
  expect(m.scrollWidth).toBeGreaterThan(m.clientWidth)
  expect(m.pageOverflow).toBeLessThanOrEqual(0)
  // The preview takes the page's width; its prose keeps the reading measure.
  expect(m.articleMax).toBe('none')
  expect(m.proseMax).not.toBe('none')
  expect(m.clientWidth).toBeGreaterThan(800)
  await page.close()
})
