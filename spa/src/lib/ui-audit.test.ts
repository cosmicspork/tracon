// The screen audit (scripts/ui-audit.mjs) is where every screen is looked at
// before a release, from the states in fixtures/states/. A screen or Settings
// section added without a state there would drop out of it unnoticed; these
// fail instead.

import { expect, test } from 'bun:test'
import { readdirSync } from 'node:fs'
import { approvalFromHash } from './device'
import { redirect, SCREENS, screenFor, SETTINGS_SECTIONS, settingsSection } from './routes'

interface AuditState {
  id: string
  area: string
  route: string
  title: string
  screen?: string
  file: string
}

/** Screens that replace the shell whatever the path, or live outside App.svelte. */
const GATES = ['login', 'device-approval', 'setup']

const dir = new URL('../../fixtures/states/', import.meta.url)
const states: AuditState[] = []
for (const file of readdirSync(dir).filter((f) => f.endsWith('.mjs')).sort()) {
  const mod = await import(new URL(file, dir).href)
  for (const s of mod.default) states.push({ ...s, file })
}

/** Where a state's address leads once the router has moved it to the current one. */
function address(s: AuditState): URL {
  const url = new URL(s.route, 'http://audit')
  const moved = redirect(url.pathname, url.search, url.hash)
  return moved ? new URL(moved, url) : url
}

function screenOf(s: AuditState): string {
  if (s.screen) return s.screen
  const url = address(s)
  if (url.pathname === '/setup.html') return 'setup'
  if (approvalFromHash(url.hash)) return 'device-approval'
  return screenFor(url.pathname).name
}

test('every state names itself, uniquely, and a screen there is', () => {
  const ids = new Set<string>()
  const problems: string[] = []
  for (const s of states) {
    if (!s.id || !s.area || !s.route || !s.title) problems.push(`${s.file}: a state without id, area, route or title`)
    if (ids.has(s.id)) problems.push(`${s.file}: ${s.id} twice`)
    ids.add(s.id)
    if (s.screen && ![...SCREENS, ...GATES].includes(s.screen)) problems.push(`${s.file}: ${s.id} names no screen ${s.screen}`)
  }
  expect(problems).toEqual([])
})

test('every screen has a state in the audit', () => {
  const covered = new Set(states.map(screenOf))
  const missing = [...SCREENS, ...GATES].filter((name) => !covered.has(name))
  // A screen here needs a state in fixtures/states/<area>.mjs.
  expect(missing).toEqual([])
})

test('every Settings section has a state in the audit', () => {
  const covered = new Set(states.filter((s) => screenOf(s) === 'settings').map((s) => settingsSection(address(s).hash)))
  const missing = SETTINGS_SECTIONS.map(([id]) => id).filter((id) => !covered.has(id))
  expect(missing).toEqual([])
})
