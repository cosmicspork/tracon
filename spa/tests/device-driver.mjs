// Drives two real Chromium pages through device approval and reports what
// it saw. The "phone" is paired and holds a passkey in Chromium's virtual
// authenticator (CDP `WebAuthn`). The "desktop" is a page with no WebAuthn at
// all, as the desktop app's webview is against a remote node: it signs in by
// having the phone approve it, then removes a passkey, which the phone
// confirms for it. The verdicts are in `node/tests/device_pwa.rs`, which
// stands the node up and runs this.
//
//   bun spa/tests/device-driver.mjs '<json config>'
//
// It prints one line, `RESULT <json>`, and exits 0 even when the flow failed
// part way: how far it got is a finding to assert on. It prints `SKIP <why>`
// when there is no browser to drive.

import { chromium } from 'playwright-core'

const config = JSON.parse(process.argv[2])
const timeout = config.timeout_ms ?? 30000

const seen = {
  enrolled: false,
  sign_in_code: null,
  sign_in_url: null,
  code_shown_to_phone: null,
  approval_headline: null,
  phone_prompt_summary: null,
  desktop_signed_in: false,
  step_up_code: null,
  step_up_url: null,
  step_up_headline: null,
  removed: false,
  failed_at: null,
  error: null,
  desktop_api: [],
  phone_page_errors: [],
  desktop_page_errors: [],
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
  // The phone: paired by QR, with a passkey.
  const phoneCtx = await browser.newContext({ viewport: { width: 390, height: 844 } })
  const phone = await phoneCtx.newPage()
  phone.on('pageerror', (e) => seen.phone_page_errors.push(String(e)))
  step = 'virtual authenticator'
  const cdp = await phoneCtx.newCDPSession(phone)
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
  step = 'pair the phone'
  await phone.goto(`${config.origin}/#token=${encodeURIComponent(config.token)}`)
  await phone.getByTestId('passkey-offer').waitFor({ timeout })
  await phone.getByTestId('offer-add-passkey').click()
  await phone.getByText('Passkey added').waitFor({ timeout })
  await phone.getByRole('button', { name: 'Done' }).click()
  seen.enrolled = true

  // The desktop: no WebAuthn, no cookie.
  const desktopCtx = await browser.newContext({ viewport: { width: 1100, height: 800 } })
  await desktopCtx.addInitScript(() => {
    delete window.PublicKeyCredential
  })
  const desktop = await desktopCtx.newPage()
  desktop.on('pageerror', (e) => seen.desktop_page_errors.push(String(e)))
  desktop.on('response', (r) => {
    const url = new URL(r.url())
    if (url.pathname.startsWith('/api/auth/device') || url.pathname.startsWith('/api/passkeys')) {
      seen.desktop_api.push({ method: r.request().method(), path: url.pathname, status: r.status() })
    }
  })

  step = 'desktop asks to sign in'
  await desktop.goto(config.origin)
  await desktop.getByTestId('device-sign-in').click()
  await desktop.getByTestId('device-code').waitFor({ timeout })
  seen.sign_in_code = (await desktop.getByTestId('device-user-code').textContent())?.trim() ?? null
  seen.sign_in_url = await desktop.getByTestId('device-open').getAttribute('href')

  // The phone opens the link (a fresh load, as a new tab would be).
  step = 'phone approves the sign-in'
  await phone.goto('about:blank')
  await phone.goto(seen.sign_in_url)
  await phone.getByTestId('device-approval').waitFor({ timeout })
  seen.approval_headline = (await phone.getByTestId('device-approval-headline').textContent())?.trim() ?? null
  // The phone does not show the code: the operator types the desktop's.
  seen.code_shown_to_phone = (await phone.getByTestId('device-approval').textContent())?.includes(seen.sign_in_code) ?? null
  await phone.getByTestId('device-approval-code').fill(seen.sign_in_code.toLowerCase())
  await phone.getByTestId('device-approve').click()
  const prompt = phone.getByTestId('step-up')
  await prompt.waitFor({ timeout })
  seen.phone_prompt_summary = (await prompt.getByTestId('step-up-summary').textContent())?.trim() ?? null
  await phone.getByTestId('step-up-confirm').click()
  await prompt.waitFor({ state: 'detached', timeout })
  await phone.getByTestId('device-approval-done').waitFor({ timeout })
  await phone.getByRole('button', { name: 'Done' }).click()

  step = 'desktop signs in'
  await desktop.getByTestId('device-sign-in').waitFor({ state: 'detached', timeout })
  await desktop.locator('nav.rail').waitFor({ timeout })
  seen.desktop_signed_in = true

  // An admin action from the desktop: it cannot make the passkey, so the
  // phone confirms it.
  step = 'desktop removes a passkey'
  await desktop.goto(`${config.origin}/settings#access`)
  await desktop.getByTestId('passkey-list').waitFor({ timeout })
  await desktop.getByTestId('passkey-list').getByRole('button', { name: 'Remove' }).first().click()
  await desktop.locator('.confirm').getByRole('button', { name: 'Remove' }).click()
  const desktopPrompt = desktop.getByTestId('step-up')
  await desktopPrompt.waitFor({ timeout })
  await desktopPrompt.getByTestId('device-code').waitFor({ timeout })
  seen.step_up_code = (await desktopPrompt.getByTestId('device-user-code').textContent())?.trim() ?? null
  seen.step_up_url = await desktopPrompt.getByTestId('device-open').getAttribute('href')

  step = 'phone confirms for the desktop'
  await phone.goto('about:blank')
  await phone.goto(seen.step_up_url)
  await phone.getByTestId('device-approval').waitFor({ timeout })
  seen.step_up_headline = (await phone.getByTestId('device-approval-headline').textContent())?.trim() ?? null
  await phone.getByTestId('device-approval-code').fill(seen.step_up_code)
  await phone.getByTestId('device-approve').click()
  await prompt.waitFor({ timeout })
  await phone.getByTestId('step-up-confirm').click()
  await prompt.waitFor({ state: 'detached', timeout })
  await phone.getByTestId('device-approval-done').waitFor({ timeout })

  step = 'desktop finishes'
  await desktopPrompt.waitFor({ state: 'detached', timeout })
  await desktop.getByText('No passkey is enrolled.').waitFor({ timeout })
  seen.removed = true
  step = 'done'
} catch (err) {
  seen.failed_at = step
  seen.error = String(err).split('\n').slice(0, 4).join(' | ')
} finally {
  await browser.close()
}

console.log(`RESULT ${JSON.stringify(seen)}`)
