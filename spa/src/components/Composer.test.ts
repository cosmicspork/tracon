// The composer's context line in a real Chromium: the middot before each fact
// keeps the space after it at every width. On a phone `.lnk` is an inline-flex
// 44px target (app.css), where a trailing space in its ::before collapses, so
// "personal · tracon · adjust" read "personal · tracon ·adjust" (#1099).
// Skipped when there is no browser to drive; set TRACON_CHROMIUM to point at
// one. In CI a missing browser fails instead.

import { afterAll, expect, setDefaultTimeout, test } from 'bun:test'
import { readFileSync } from 'node:fs'

import { chromium, type Browser } from 'playwright-core'

const appCss = readFileSync(new URL('../app.css', import.meta.url), 'utf8')
// The component's own rules, unscoped: the markup below carries the same
// classes, so they match it as they match the mounted component.
const composerCss = /<style>([\s\S]*?)<\/style>/.exec(
  readFileSync(new URL('./Composer.svelte', import.meta.url), 'utf8'),
)![1]

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
if (!browser && process.env.CI) throw new Error('Composer.test.ts: no Chromium launched in CI; set TRACON_CHROMIUM or install Chrome')
afterAll(() => browser?.close())
setDefaultTimeout(20000)

// How far each fact's text starts from its box's left edge: the drawn "· " and
// whatever space follows it. The first fact has none.
async function lead(width: number): Promise<{ text: string; lead: number }[]> {
  const page = await browser!.newPage({ viewport: { width, height: 400 } })
  await page.setContent(
    `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width">` +
      `<style>${appCss}</style><style>${composerCss}</style>` +
      `<body style="margin:0;padding:0 16px"><div class="line"><div class="ctx">` +
      `<span>personal</span><span>tracon</span><button type="button" class="lnk">adjust</button>` +
      `</div><button class="btn p">Start session</button></div></body>`,
  )
  const out = await page.evaluate(() =>
    [...document.querySelectorAll('.ctx > *')].map((el) => {
      const range = document.createRange()
      range.selectNodeContents(el.firstChild!)
      return { text: el.textContent!, lead: range.getBoundingClientRect().left - el.getBoundingClientRect().left }
    }),
  )
  await page.close()
  return out
}

for (const [name, width] of [
  ['phone', 390],
  ['desktop', 1280],
] as const) {
  test.skipIf(!browser)(`on a ${name} the link's middot keeps its space, as the facts' do`, async () => {
    const [first, fact, link] = await lead(width)
    expect(first.lead).toBe(0)
    // A middot and a space in the 12px mono face, not a middot alone.
    expect(fact.lead).toBeGreaterThan(12)
    expect(Math.abs(link.lead - fact.lead)).toBeLessThanOrEqual(1)
  })
}
