// Drives a real Chromium through pairing, passkey enrollment, and a remote
// admin action confirmed with that passkey, and reports what it saw. The
// authenticator is Chromium's own virtual one (CDP `WebAuthn` domain), so the
// ceremony is the browser's: its client data, its origin check, its CBOR.
// The verdicts are in `node/tests/passkey_pwa.rs`, which stands the node up
// and runs this.
//
//   bun spa/tests/passkey-driver.mjs '<json config>'
//
// It prints one line, `RESULT <json>`, and exits 0 even when the page failed
// part way: how far it got is a finding to assert on. It prints `SKIP <why>`
// when there is no browser to drive.

import { chromium } from 'playwright-core'

const config = JSON.parse(process.argv[2])
const timeout = config.timeout_ms ?? 20000

const seen = {
  offered: false,
  enrolled: false,
  authenticator_credentials: null,
  listed: null,
  step_up_action: null,
  step_up_summary: null,
  removed: false,
  failed_at: null,
  error: null,
  api: [],
  console: [],
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
  // Phone-sized, as the PWA this is for.
  const ctx = await browser.newContext({ viewport: { width: 390, height: 844 } })
  const page = await ctx.newPage()
  page.on('console', (m) => seen.console.push(m.text()))
  page.on('pageerror', (e) => seen.page_errors.push(String(e)))
  page.on('response', (r) => {
    const url = new URL(r.url())
    if (url.pathname.startsWith('/api/passkeys') || url.pathname === '/api/login') {
      seen.api.push({ method: r.request().method(), path: url.pathname, status: r.status() })
    }
  })

  step = 'virtual authenticator'
  const cdp = await ctx.newCDPSession(page)
  await cdp.send('WebAuthn.enable')
  const { authenticatorId } = await cdp.send('WebAuthn.addVirtualAuthenticator', {
    options: {
      protocol: 'ctap2',
      transport: 'internal',
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  })

  // The pairing QR's address: the token rides in the fragment.
  step = 'pair'
  await page.goto(`${config.origin}/#token=${encodeURIComponent(config.token)}`)
  await page.getByTestId('passkey-offer').waitFor({ timeout })
  seen.offered = true

  step = 'enroll'
  await page.getByTestId('offer-add-passkey').click()
  await page.getByText('Passkey added').waitFor({ timeout })
  seen.enrolled = true
  const creds = await cdp.send('WebAuthn.getCredentials', { authenticatorId })
  seen.authenticator_credentials = creds.credentials.length
  await page.getByRole('button', { name: 'Done' }).click()

  step = 'list'
  await page.goto(`${config.origin}/settings#access`)
  await page.getByTestId('passkey-list').waitFor({ timeout })
  seen.listed = await page.getByTestId('passkey-list').locator('.key').count()

  // Removing a passkey is an admin action: away from the node it is refused
  // until the step-up prompt confirms it with the passkey itself.
  step = 'remove'
  await page.getByTestId('passkey-list').getByRole('button', { name: 'Remove' }).first().click()
  await page.locator('.confirm').getByRole('button', { name: 'Remove' }).click()
  step = 'step-up'
  const prompt = page.getByTestId('step-up')
  await prompt.waitFor({ timeout })
  seen.step_up_action = (await prompt.locator('.action').textContent())?.trim() ?? null
  seen.step_up_summary = (await prompt.getByTestId('step-up-summary').textContent())?.trim() ?? null
  await page.getByTestId('step-up-confirm').click()
  await prompt.waitFor({ state: 'detached', timeout })
  step = 'removed'
  await page.getByText('No passkey is enrolled.').waitFor({ timeout })
  seen.removed = true
  step = 'done'
} catch (err) {
  seen.failed_at = step
  seen.error = String(err).split('\n').slice(0, 4).join(' | ')
} finally {
  await browser.close()
}

console.log(`RESULT ${JSON.stringify(seen)}`)
