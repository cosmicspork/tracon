// Drives a real Chromium, phone-sized and off the node's machine, through
// pairing, passkey enrollment, and one remote edit of a live node.toml key in
// Settings › Configuration, confirmed with that passkey. The authenticator is
// Chromium's virtual one (CDP `WebAuthn` domain). The verdicts, including
// that the running node took the value, are in `node/tests/config_pwa.rs`,
// which stands the node up and runs this.
//
//   bun spa/tests/config-driver.mjs '<json config>'
//
// It prints one line, `RESULT <json>`, and exits 0 even when the page failed
// part way: how far it got is a finding to assert on. It prints `SKIP <why>`
// when there is no browser to drive.

import { chromium } from 'playwright-core'

const config = JSON.parse(process.argv[2])
const timeout = config.timeout_ms ?? 20000

const seen = {
  enrolled: false,
  badge: null,
  step_up_summary: null,
  saved_note: null,
  overflow: null,
  failed_at: null,
  error: null,
  api: [],
  page_errors: [],
}

let browser
try {
  browser = await chromium.launch(config.executable ? { executablePath: config.executable } : {})
} catch (err) {
  console.log(`SKIP no Chromium to drive: ${String(err).split('\n')[0]}`)
  process.exit(0)
}

let step = 'start'
try {
  const ctx = await browser.newContext({ viewport: { width: 390, height: 844 } })
  const page = await ctx.newPage()
  page.on('pageerror', (e) => seen.page_errors.push(String(e)))
  page.on('response', (r) => {
    const url = new URL(r.url())
    if (url.pathname === '/api/config') seen.api.push({ method: r.request().method(), status: r.status() })
  })

  step = 'virtual authenticator'
  const cdp = await ctx.newCDPSession(page)
  await cdp.send('WebAuthn.enable')
  await cdp.send('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol: 'ctap2',
      transport: 'internal',
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  })

  step = 'pair and enroll'
  await page.goto(`${config.origin}/#token=${encodeURIComponent(config.token)}`)
  await page.getByTestId('passkey-offer').waitFor({ timeout })
  await page.getByTestId('offer-add-passkey').click()
  await page.getByText('Passkey added').waitFor({ timeout })
  seen.enrolled = true
  await page.getByRole('button', { name: 'Done' }).click()

  step = 'open the form'
  await page.goto(`${config.origin}/settings#config`)
  await page.getByTestId('config-form').waitFor({ timeout })
  await page.getByLabel('Find a key').fill(config.key)
  const input = page.locator(`#cfg-${config.key.replace(/\./g, '-')}`)
  await input.waitFor({ timeout })
  seen.overflow = await page.evaluate(() => document.documentElement.scrollWidth > document.documentElement.clientWidth)
  seen.badge = (await page.locator(`[data-key="${config.key}"] .badge`).textContent())?.trim() ?? null

  step = 'edit'
  await input.fill(String(config.value))
  await page.getByRole('button', { name: `Save [${config.key.split('.')[0]}]` }).click()

  step = 'step-up'
  const prompt = page.getByTestId('step-up')
  await prompt.waitFor({ timeout })
  seen.step_up_summary = (await prompt.getByTestId('step-up-summary').textContent())?.trim() ?? null
  await page.getByTestId('step-up-confirm').click()
  await prompt.waitFor({ state: 'detached', timeout })

  step = 'saved'
  const note = page.locator('.note.ok')
  await note.waitFor({ timeout })
  seen.saved_note = (await note.textContent())?.trim() ?? null
  step = 'done'
} catch (err) {
  seen.failed_at = step
  seen.error = String(err).split('\n').slice(0, 4).join(' | ')
} finally {
  await browser.close()
}

console.log(`RESULT ${JSON.stringify(seen)}`)
