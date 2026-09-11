/*
 * Executes only the structured BrowserScenario in a configured, digest-pinned
 * Playwright runtime. It never accepts code, command text, URLs, or credentials
 * from its JSON input. Screenshot bytes stay in the runtime workspace and are
 * exported by the node's guarded volume transfer after the browser exits.
 */
'use strict'

const fs = require('node:fs/promises')
const path = require('node:path')
const { chromium } = require('playwright')

const [, , specPath] = process.argv
const OUTPUT = '/work/output'
const SCREENSHOTS = `${OUTPUT}/screenshots`
const MAX_LOG = 64 * 1024

function appendLog(lines, value) {
  const next = String(value).replace(/[\r\n]+/g, ' ').slice(0, 2048)
  const joined = `${lines.join('\n')}\n${next}`
  if (joined.length <= MAX_LOG) {
    lines.push(next)
  } else {
    lines.splice(0, lines.length, joined.slice(-MAX_LOG))
  }
}

function originAllowed(value, allowed) {
  try {
    const url = new URL(value)
    return (url.protocol === 'https:' || url.protocol === 'http:') && allowed.has(url.origin)
  } catch {
    return false
  }
}

function assertion(kind, ok, detail) {
  return { kind, ok: Boolean(ok), detail: String(detail).slice(0, 2048) }
}

async function screenshot(page, label) {
  const file = `${SCREENSHOTS}/${label}.png`
  await page.screenshot({ path: file, type: 'png', fullPage: true })
  return { label, path: `output/screenshots/${label}.png` }
}

async function main() {
  const spec = JSON.parse(await fs.readFile(specPath, 'utf8'))
  const allowed = new Set(spec.allowed_origins)
  if (!originAllowed(spec.start_url, allowed)) throw new Error('start URL is outside approved origins')
  if (!Array.isArray(spec.steps) || !Array.isArray(spec.assertions)) throw new Error('malformed scenario')
  await fs.mkdir(SCREENSHOTS, { recursive: true })

  const log = []
  const results = []
  const screenshots = []
  // WebRTC's own ICE/STUN candidate gathering is not HTTP(S)/WS traffic and
  // is invisible to `context.route`/`routeWebSocket` below; these two flags
  // are Chromium's own switch to stop it from ever dialling a candidate
  // outside the configured proxy (`disable_non_proxied_udp` refuses direct
  // UDP entirely once a proxy is set).
  const browser = await chromium.launch({
    headless: true,
    proxy: spec.proxy_url ? { server: spec.proxy_url } : undefined,
    args: [
      '--force-webrtc-ip-handling-policy=disable_non_proxied_udp',
      '--disable-features=WebRtcHideLocalIpsWithMdns',
    ],
  })
  let page
  try {
    // Blocking service workers prevents cached or service-worker initiated
    // traffic from escaping the route controls below.
    const context = await browser.newContext({ serviceWorkers: 'block' })
    await context.route('**/*', async route => {
      const url = route.request().url()
      if (originAllowed(url, allowed)) {
        await route.continue()
      } else {
        appendLog(log, `blocked request ${url}`)
        await route.abort('blockedbyclient')
      }
    })
    if (typeof context.routeWebSocket !== 'function') {
      throw new Error('browser runtime lacks WebSocket interception')
    }
    await context.routeWebSocket('**/*', ws => {
      const url = ws.url()
      if (originAllowed(url, allowed)) {
        ws.connect()
      } else {
        appendLog(log, `blocked websocket ${url}`)
        ws.close()
      }
    })
    page = await context.newPage()
    page.on('console', message => appendLog(log, `console ${message.type()}: ${message.text()}`))
    page.on('pageerror', error => appendLog(log, `pageerror: ${error.message}`))
    page.on('requestfailed', request => appendLog(log, `request failed: ${request.url()} ${request.failure()?.errorText ?? ''}`))

    await page.goto(spec.start_url, { waitUntil: 'networkidle', timeout: spec.timeout_ms })
    if (!originAllowed(page.url(), allowed)) throw new Error('initial navigation left approved origins')
    for (const step of spec.steps) {
      try {
        if (step.kind === 'navigate') {
          if (!originAllowed(step.url, allowed)) throw new Error('navigation escaped approved origins')
          await page.goto(step.url, { waitUntil: 'networkidle', timeout: spec.timeout_ms })
        } else if (step.kind === 'click') {
          await page.locator(step.selector).click({ timeout: spec.timeout_ms })
        } else if (step.kind === 'fill') {
          const locator = page.locator(step.selector)
          if (step.credential_env) {
            if (new URL(page.url()).origin !== new URL(spec.start_url).origin) {
              throw new Error('dedicated test credentials may be filled only on the primary QA origin')
            }
            // Masking is not a promise the value stays off screen — a
            // misconfigured selector could point at a plain text input that
            // echoes it. A password-type input is the one element type a
            // full-page screenshot cannot render legibly, so it is the only
            // one a credential is ever filled into.
            const inputType = await locator.evaluate(el => (el.getAttribute('type') || '').toLowerCase())
            if (inputType !== 'password') {
              throw new Error('a dedicated test credential may be filled only into a password-type input')
            }
          }
          const value = step.credential_env ? process.env[step.credential_env] : step.value
          if (typeof value !== 'string') throw new Error('dedicated test credential is unavailable')
          await locator.fill(value, { timeout: spec.timeout_ms })
        } else if (step.kind === 'wait_for') {
          await page.locator(step.selector).waitFor({ state: 'visible', timeout: spec.timeout_ms })
        } else {
          throw new Error('unknown browser step')
        }
        if (!originAllowed(page.url(), allowed)) {
          throw new Error('browser step left approved origins')
        }
      } catch (error) {
        results.push(assertion(`step:${step.kind}`, false, error.message))
        throw error
      }
    }

    for (const check of spec.assertions) {
      try {
        if (check.kind === 'url_path_is') {
          const current = new URL(page.url())
          results.push(assertion(check.kind, current.pathname === check.path, current.pathname))
        } else if (check.kind === 'title_contains') {
          const title = await page.title()
          results.push(assertion(check.kind, title.includes(check.text), title))
        } else if (check.kind === 'text_visible') {
          const locator = page.locator(check.selector).filter({ hasText: check.text }).first()
          results.push(assertion(check.kind, await locator.isVisible({ timeout: spec.timeout_ms }), check.selector))
        } else if (check.kind === 'element_count') {
          const count = await page.locator(check.selector).count()
          results.push(assertion(check.kind, count === check.count, `${count} elements`))
        } else if (check.kind === 'screenshot') {
          screenshots.push(await screenshot(page, check.label))
          results.push(assertion(check.kind, true, check.label))
        } else {
          results.push(assertion('unknown', false, 'unknown assertion'))
        }
      } catch (error) {
        results.push(assertion(check.kind, false, error.message))
      }
    }
  } catch (error) {
    appendLog(log, `runner: ${error.message}`)
    results.push(assertion('runner', false, error.message))
  } finally {
    await browser.close()
  }

  const report = {
    ok: results.length > 0 && results.every(result => result.ok),
    final_url: page ? page.url() : '',
    assertions: results,
    screenshots,
    log: log.join('\n').slice(-MAX_LOG),
  }
  await fs.writeFile(`${OUTPUT}/report.json`, JSON.stringify(report), { mode: 0o600 })
  // Only this path is named on stdout; report bytes and screenshots are copied
  // through the workspace transfer, never into a process transcript.
  process.stdout.write('output/report.json\n')
}

main().catch(async error => {
  await fs.mkdir(OUTPUT, { recursive: true })
  await fs.writeFile(`${OUTPUT}/report.json`, JSON.stringify({
    ok: false,
    final_url: '',
    assertions: [assertion('runner', false, error.message)],
    screenshots: [],
    log: `runner: ${error.message}`.slice(-MAX_LOG),
  }), { mode: 0o600 })
  process.stdout.write('output/report.json\n')
  process.exitCode = 1
})
