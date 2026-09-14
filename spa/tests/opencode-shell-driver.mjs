// Drives a real Chromium at phone size over the installed app's OpenCode
// shell, and reports what it saw. Everything it asserts about is here as an
// observation rather than a verdict: the verdicts are in
// `node/tests/opencode_pwa_shell.rs`, which stands the two origins up and runs
// this, so a browser result and a server result are read together.
//
//   bun spa/tests/opencode-shell-driver.mjs '<json config>'
//
// It prints one line, `RESULT <json>`, and exits 0 even when the page failed:
// "the frame was refused" is a finding to assert on, not a crash. It exits
// non-zero only when the browser itself could not be driven.
//
// Why playwright-core and not raw CDP: the package is already a dev dependency
// of this app, and the browser it drives is the one Playwright installs. A
// machine with neither is told so by the Rust test and the case is skipped,
// the same rule the pinned-binary tests apply.

import { chromium } from 'playwright-core'

const config = JSON.parse(process.argv[2])
const timeout = config.timeout_ms ?? 30000

/** Everything the browser did that the assertions are read from. */
const seen = {
  shell_url: null,
  in_scope: false,
  frames: [],
  ui_requests: [],
  console: [],
  page_errors: [],
  overflow: null,
  frame_attrs: null,
  ui_cookies: [],
  frame_text: null,
  reconnect_visible: null,
  cookie_events: [],
}

let browser
try {
  browser = await chromium.launch({
    headless: true,
    executablePath: chromium.executablePath(),
    args: [
    '--no-sandbox',
    '--hide-scrollbars',
    // The whole point of the run. Without this the cookie would be accepted
    // for the ordinary reason and would prove nothing about a phone whose
    // browser blocks third-party storage — which is the default the installed
    // app has to work under.
      ...(config.block_third_party ? ['--test-third-party-cookie-phaseout'] : []),
    ],
  })
} catch (e) {
  // No browser on this machine is a skip, not a failure — the same rule the
  // pinned-binary tests apply. The caller decides; this only reports.
  console.log(`SKIP ${String(e).replace(/[\r\n]+/g, ' ').slice(0, 300)}`)
  process.exit(0)
}

try {
  const context = await browser.newContext({
    viewport: { width: config.width ?? 390, height: config.height ?? 844 },
    deviceScaleFactor: 3,
  })
  const page = await context.newPage()

  // Chromium's own account of what it did with each cookie, which is the only
  // place a *silent* refusal shows up: a `Set-Cookie` the browser declined
  // looks exactly like one the server never sent, from the page's side.
  const cdp = await context.newCDPSession(page)
  await cdp.send('Network.enable')
  cdp.on('Network.responseReceivedExtraInfo', (e) => {
    const set = e.headers && (e.headers['set-cookie'] || e.headers['Set-Cookie'])
    if (!set && !(e.blockedCookies || []).length) return
    seen.cookie_events.push({
      set_cookie: set ? String(set).replace(/=[^;]{8,}/, '=<redacted>') : null,
      blocked: (e.blockedCookies || []).map((c) => c.blockedReasons).flat(),
      partition_key: e.cookiePartitionKey ?? null,
    })
  })

  page.on('console', (m) => seen.console.push(`${m.type()}: ${m.text()}`.slice(0, 400)))
  page.on('pageerror', (e) => seen.page_errors.push(String(e).slice(0, 400)))
  page.on('response', (res) => {
    const url = res.url()
    if (!url.startsWith(config.ui_origin)) return
    seen.ui_requests.push({
      method: res.request().method(),
      path: new URL(url).pathname,
      status: res.status(),
    })
  })

  await page.goto(config.shell_url, { waitUntil: 'domcontentloaded', timeout })

  // The frame is created once the shell has minted a capability, which is one
  // round trip to the node. Waiting on the element rather than on a delay.
  await page
    .waitForSelector('iframe.view', { timeout })
    .catch(() => seen.page_errors.push('no iframe appeared'))

  // And the exchange inside it is a second round trip. Wait for the node to
  // have answered `/boot` rather than for the frame to look a certain way.
  await page
    .waitForResponse((r) => r.url() === `${config.ui_origin}/boot`, { timeout })
    .catch(() => seen.page_errors.push('the frame never reached /boot'))

  if (config.settle_ms) await page.waitForTimeout(config.settle_ms)

  // Whatever the shell is actually showing, so a failure reads as a sentence
  // rather than as an absent element.
  seen.main_text = await page.evaluate(() => document.body.innerText.slice(0, 600))

  seen.shell_url = page.url()
  const shell = new URL(seen.shell_url)
  seen.in_scope =
    shell.origin === config.operator_origin && shell.pathname.startsWith(config.scope ?? '/')

  seen.frames = page
    .frames()
    .filter((f) => f !== page.mainFrame())
    .map((f) => ({ origin: safeOrigin(f.url()), path: safePath(f.url()) }))

  seen.frame_attrs = await page.evaluate(() => {
    const el = document.querySelector('iframe.view')
    if (!el) return null
    return {
      sandbox: el.getAttribute('sandbox'),
      allow: el.getAttribute('allow'),
      referrerpolicy: el.getAttribute('referrerpolicy'),
      // The capability is in the attribute and must not be anywhere else.
      src_has_fragment: (el.getAttribute('src') || '').includes('#'),
    }
  })

  // A phone screen that scrolls sideways is a broken screen. Measured on the
  // real layout at the real width rather than reasoned about from the CSS.
  seen.overflow = await page.evaluate(() => ({
    scroll_width: document.documentElement.scrollWidth,
    client_width: document.documentElement.clientWidth,
    body_scroll_width: document.body.scrollWidth,
  }))

  // Whether the shell's own address bar ever carried the capability. It must
  // not: the fragment belongs to the frame, and this page's history is a thing
  // the browser keeps.
  seen.shell_url_has_boot = seen.shell_url.includes('boot')

  const frame = page.frames().find((f) => f.url().startsWith(config.ui_origin))
  if (frame) {
    seen.frame_text = await frame
      .evaluate(() => document.body.innerText.slice(0, 400))
      .catch(() => null)
  }

  seen.ui_cookies = (await context.cookies([config.ui_origin])).map((c) => ({
    name: c.name,
    sameSite: c.sameSite,
    secure: c.secure,
    httpOnly: c.httpOnly,
    partitionKey: c.partitionKey ?? null,
  }))

  // Resume: a phone backgrounds the app far more often than it closes it. The
  // shell re-checks the session on `visibilitychange`, so this fires the same
  // event the operating system would and reports what the shell then shows.
  if (config.resume) {
    // What happened while the phone was asleep. The caller decides; this only
    // asks the node to make it true, from outside the page, before the page is
    // told it is visible again.
    if (config.before_resume_url) {
      const r = await page.request.post(config.before_resume_url)
      seen.before_resume_status = r.status()
    }
    await page.evaluate(() => {
      document.dispatchEvent(new Event('visibilitychange'))
      window.dispatchEvent(new Event('pageshow'))
    })
    await page.waitForTimeout(config.resume_settle_ms ?? 1500)
    seen.reconnect_visible = await page.evaluate(() =>
      Array.from(document.querySelectorAll('button')).some(
        (b) => b.textContent.trim() === 'Reconnect',
      ),
    )
    seen.back_link_visible = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.msg a')).some(
        (a) => a.textContent.trim() === 'Back to the session',
      ),
    )
    seen.frame_still_there = await page.evaluate(() => Boolean(document.querySelector('iframe.view')))
    seen.after_resume_text = await page.evaluate(() =>
      document.querySelector('.msg') ? document.querySelector('.msg').innerText.slice(0, 300) : null,
    )
    seen.after_resume_url = page.url()
  }

  if (config.screenshot) {
    await page.screenshot({ path: config.screenshot, fullPage: false })
  }
} finally {
  await browser.close()
}

function safeOrigin(url) {
  try {
    return new URL(url).origin
  } catch {
    return url
  }
}

function safePath(url) {
  try {
    return new URL(url).pathname
  } catch {
    return url
  }
}

console.log(`RESULT ${JSON.stringify(seen)}`)
