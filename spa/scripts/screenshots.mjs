// The README's screenshots, from the fixture mode — no node, no boundary.
//
//   cd spa && bun install && bun scripts/screenshots.mjs
//
// Starts `vite` with TRACON_FIXTURES=1, captures each screen at desktop and
// phone widths into docs/media/, and exits. Chromium comes from Playwright
// (PLAYWRIGHT_BROWSERS_PATH, or the default install).

import { spawn } from 'node:child_process'
import { mkdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

import { chromium } from 'playwright-core'

const port = 5199
const base = `http://127.0.0.1:${port}`
const out = fileURLToPath(new URL('../../docs/media/', import.meta.url))
mkdirSync(out, { recursive: true })

const shots = [
  { path: '/', name: 'home' },
  { path: '/sessions/s-run', name: 'session' },
  { path: '/work', name: 'work' },
  { path: '/nodes', name: 'nodes' },
  { path: '/settings', name: 'settings' },
]
const sizes = [
  { name: 'desktop', width: 1280, height: 800 },
  { name: 'phone', width: 390, height: 844 },
]

const vite = spawn(
  'bun',
  // Bound explicitly: vite otherwise picks the host's default, which on some
  // machines is IPv6-only, and the wait below dials 127.0.0.1.
  ['run', 'dev', '--', '--port', String(port), '--strictPort', '--host', '127.0.0.1'],
  {
    cwd: fileURLToPath(new URL('..', import.meta.url)),
    env: { ...process.env, TRACON_FIXTURES: '1' },
    stdio: 'ignore',
  },
)
process.on('exit', () => vite.kill())

for (let i = 0; ; i++) {
  try {
    await fetch(`${base}/api/node`)
    break
  } catch {
    if (i > 60) throw new Error('vite did not come up')
    await new Promise((r) => setTimeout(r, 250))
  }
}

const executablePath = process.env.TRACON_CHROMIUM // e.g. /opt/pw-browsers/chromium
const browser = await chromium.launch(executablePath ? { executablePath } : {})
for (const size of sizes) {
  const ctx = await browser.newContext({
    viewport: { width: size.width, height: size.height },
    colorScheme: 'dark',
    deviceScaleFactor: 2,
  })
  const page = await ctx.newPage()
  for (const shot of shots) {
    await page.goto(`${base}${shot.path}`, { waitUntil: 'networkidle' })
    await page.waitForTimeout(400)
    await page.screenshot({ path: `${out}${shot.name}-${size.name}.png` })
    console.log(`${shot.name}-${size.name}.png`)
  }
  await ctx.close()
}
await browser.close()
vite.kill()
process.exit(0)
