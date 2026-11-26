// The review page's forge text in a real Chromium: edited in On the forge's
// Source tab, marked edited, diffed under Changes, and cleared back to the
// agent's text, which the draft saves so a reload finds it cleared. Skipped
// when there is no browser to drive; set TRACON_CHROMIUM to point at one. In
// CI a missing browser fails instead.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium, type Browser, type Page } from 'playwright-core'
import { compile } from 'svelte/compiler'

import { appCss } from '../lib/app-css'
import type { ReviewDraft } from '../lib/types'

const here = fileURLToPath(new URL('.', import.meta.url))

const ID = '01a21416-7458-77b0-a822-408450a14cc8'
const harness = `
  <script>
    import Review from ${JSON.stringify(`${here}Review.svelte`)}
  </script>
  <Review id="${ID}" />
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
if (!browser && process.env.CI) throw new Error('ReviewForge.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

const details = {
  review: {
    id: ID,
    session_id: null,
    lane: 'an agent',
    node_id: 'n',
    channel: 'main',
    kind: 'review',
    title: 'Fix the thing',
    body: 'This fixes the thing.',
    edited_title: null,
    edited_body: null,
    provider: 'github',
    target: JSON.stringify({ project: 'o/r', branch: 'fix', base: 'main' }),
    diff: 'diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-a\n+b\n',
    files: JSON.stringify([{ path: 'a.txt', blob: 'x' }]),
    head_sha: 'abcdef1234567890',
    base_ref: 'main',
    added: 1,
    removed: 1,
    state: 'new',
    verdict_reason: null,
    publish_result: null,
    claimed_ms: null,
    created_ms: Date.now(),
    checks_json: null,
    review_session_id: null,
    ai_verdict_json: null,
  },
  revision: { id: 'rev2', head_sha: 'abcdef1234567890', created_ms: 0 },
  stale: [],
  evidence: null,
  requirements: null,
  intent: { forge: {} },
  shown_work: [],
}

/** The review page with `held` as the node's draft; what it saves lands in `saved`. */
async function open(held: ReviewDraft | null): Promise<{ page: Page; saved: unknown[] }> {
  const page = await browser!.newPage({ viewport: { width: 1100, height: 1200 } })
  const errors: string[] = []
  const saved: unknown[] = []
  page.on('pageerror', (e) => errors.push(String(e)))
  const js = (await bundle()).replaceAll('</script', '<\\/script')
  const html =
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width"><style>${appCss()}</style>` +
    `<body style="margin:0;padding:0 16px"><div id="app"></div><script type="module">${js}</script></body>`
  await page.route('http://tracon.test/**', (route) => {
    const req = route.request()
    const path = new URL(req.url()).pathname
    const json = (body: unknown) => route.fulfill({ contentType: 'application/json', body: JSON.stringify(body) })
    if (path === `/api/reviews/${ID}`) return json(details)
    if (path === `/api/reviews/${ID}/draft`) {
      if (req.method() === 'GET') return json({ draft: held })
      saved.push(req.postDataJSON())
      return json({ version: saved.length + 1, updated_ms: 1 })
    }
    if (path === `/api/reviews/${ID}/outgoing`) return route.fulfill({ status: 500, body: 'no' })
    if (path.startsWith('/api/')) return json({})
    return route.fulfill({ contentType: 'text/html', body: html })
  })
  await page.goto('http://tracon.test/')
  await page.waitForSelector('text=On the forge', { timeout: 5000 }).catch((e) => {
    throw new Error(errors.join('\n') || String(e))
  })
  return { page, saved }
}

const chip = (page: Page) => page.locator('.out .chip', { hasText: 'edited' })

test.skipIf(!browser)('an edit in Source shows the chip and a word diff, and Clear edits saves the agent text back', async () => {
  const { page, saved } = await open(null)
  expect(await page.getByText('Title and body').count()).toBe(0)
  expect(await chip(page).count()).toBe(0)
  await page.getByRole('tab', { name: 'Changes' }).click()
  expect(await page.getByText('No edits').isVisible()).toBe(true)
  await page.getByRole('tab', { name: 'Source' }).click()
  await page.getByLabel('body', { exact: true }).fill('This fixes the thing properly.')
  expect(await chip(page).isVisible()).toBe(true)
  // Saved as a draft, as every edit is.
  await page.waitForFunction(() => document.querySelector('.draft-state')?.textContent === 'draft saved')
  expect((saved.at(-1) as { draft: { body: string } }).draft.body).toBe('This fixes the thing properly.')
  await page.getByRole('tab', { name: 'Changes' }).click()
  expect(await page.locator('.out ins').textContent()).toContain('properly')
  await page.getByRole('button', { name: 'Clear edits' }).click()
  expect(await chip(page).count()).toBe(0)
  expect(await page.getByText('No edits').isVisible()).toBe(true)
  // The cleared text is saved as the draft, so a reload and other devices see it.
  const before = saved.length
  await page.waitForTimeout(900)
  expect(saved.length).toBe(before + 1)
  const last = saved[saved.length - 1] as { draft: { title: string; body: string } }
  expect(last.draft.title).toBe('Fix the thing')
  expect(last.draft.body).toBe('This fixes the thing.')
  await page.close()
})

test.skipIf(!browser)('a draft from an earlier revision names what it overrides, and opens Changes', async () => {
  const { page } = await open({
    review_id: ID,
    revision_id: 'rev1',
    version: 1,
    updated_ms: 1,
    draft: { title: 'Fix the thing better' },
  })
  const banner = page.locator('.draft-earlier')
  expect(await banner.textContent()).toContain('your edits are from an earlier revision · the agent changed title since')
  expect(await chip(page).isVisible()).toBe(true)
  // The footer no longer carries it.
  expect(await page.locator('.draft-state').textContent()).toBe('draft saved')
  await banner.getByRole('button', { name: 'Show changes' }).click()
  expect(await page.getByRole('tab', { name: 'Changes' }).getAttribute('aria-selected')).toBe('true')
  expect(await page.locator('.out ins').textContent()).toContain('better')
  await banner.getByRole('button', { name: 'Clear edits' }).click()
  expect(await banner.count()).toBe(0)
  await page.close()
})

test.skipIf(!browser)('a draft on this revision shows the chip but no banner', async () => {
  const { page } = await open({ review_id: ID, revision_id: 'rev2', version: 1, updated_ms: 1, draft: { title: 'Mine' } })
  expect(await chip(page).isVisible()).toBe(true)
  expect(await page.locator('.draft-earlier').count()).toBe(0)
  await page.close()
})
