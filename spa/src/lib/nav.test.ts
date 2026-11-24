import { expect, test } from 'bun:test'
import {
  activeKey,
  addPlugin,
  addable,
  defaultNav,
  drawNav,
  entryKey,
  isDefaultNav,
  loadNav,
  moveEntry,
  normalizeNav,
  phoneNav,
  removePlugin,
  saveNav,
  setHidden,
  type NavEntry,
} from './nav'
import type { PluginSummary } from './types'

const plugin = (name: string, o: Partial<PluginSummary> & { icon?: boolean } = {}): PluginSummary => ({
  name,
  repo: `cosmicspork/${name}`,
  state: 'active',
  active: { version: '0.4.0', digest: `d-${name}`, tools: [], ui: true, ui_title: name.toUpperCase(), icon: o.icon ?? true },
  instances: [],
  offered_on: ['work'],
  ...o,
})

const keys = (list: { key?: string }[] | NavEntry[]) =>
  (list as (NavEntry | { key: string })[]).map((e) => ('key' in e ? e.key : entryKey(e as NavEntry)))

const memory = () => {
  const m = new Map<string, string>()
  return {
    m,
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  }
}

test("nothing saved is today's navigation, everything shown, no plugin entries", () => {
  const s = memory()
  const list = loadNav('n1', s)
  expect(keys(list)).toEqual(['home', 'tasks', 'sessions', 'docs', 'memories', 'plugins', 'usage'])
  expect(isDefaultNav(list)).toBe(true)
  expect(keys(drawNav(list, []))).toEqual(['home', 'tasks', 'sessions', 'docs', 'memories', 'plugins', 'usage'])
})

test('the order can change and is kept per node', () => {
  const s = memory()
  let list = moveEntry(defaultNav(), 6, -6) // Usage to the top
  list = moveEntry(list, 1, 1) // Home below Tasks
  expect(keys(list).slice(0, 3)).toEqual(['usage', 'tasks', 'home'])
  saveNav('n1', list, s)
  expect(keys(loadNav('n1', s))).toEqual(keys(list))
  expect(isDefaultNav(loadNav('n2', s))).toBe(true)
  // Out of bounds changes nothing.
  expect(moveEntry(list, 0, -1)).toBe(list)
  expect(moveEntry(list, list.length - 1, 1)).toBe(list)
})

test('entries can be hidden and shown again; Home cannot be hidden', () => {
  let list = setHidden(defaultNav(), 4, true) // Memories
  expect(keys(drawNav(list, []))).not.toContain('memories')
  list = setHidden(list, 4, false)
  expect(keys(drawNav(list, []))).toContain('memories')
  expect(setHidden(list, 0, true)).toBe(list)
  // Not even from storage.
  const restored = normalizeNav([{ kind: 'builtin', id: 'home', hidden: true }])
  expect(restored[0]).toEqual({ kind: 'builtin', id: 'home' })
})

test('Settings is fixed: never in the list, whatever is stored', () => {
  const list = normalizeNav([{ kind: 'builtin', id: 'settings' }, { kind: 'builtin', id: 'usage' }])
  expect(keys(list)).not.toContain('settings')
  // Missing built-ins come back where today's order has them.
  expect(keys(list)).toEqual(['home', 'tasks', 'sessions', 'docs', 'memories', 'plugins', 'usage'])
  expect(normalizeNav('nonsense')).toEqual(defaultNav())
})

test('plugin entries are added and removed, once each, and reset restores the default', () => {
  const s = memory()
  let list = addPlugin(defaultNav(), 'workboard')
  expect(addPlugin(list, 'workboard')).toBe(list)
  const plugins = [plugin('workboard'), plugin('hoja')]
  expect(addable(list, plugins).map((p) => p.name)).toEqual(['hoja'])
  const drawn = drawNav(list, plugins)
  expect(drawn.at(-1)).toMatchObject({
    key: 'plugin:workboard',
    label: 'WORKBOARD',
    href: '/plugins/workboard',
    icon: '/api/plugins/workboard/icon?v=d-workboard',
  })
  saveNav('n1', list, s)
  expect(keys(loadNav('n1', s))).toContain('plugin:workboard')
  list = removePlugin(list, 'workboard')
  expect(keys(list)).not.toContain('plugin:workboard')
  saveNav('n1', defaultNav(), s)
  expect(s.m.has('tracon-nav:n1')).toBe(false)
})

test('a plugin entry is skipped while its plugin is missing, and back once it returns', () => {
  const list = addPlugin(defaultNav(), 'workboard')
  expect(keys(drawNav(list, []))).not.toContain('plugin:workboard')
  expect(keys(drawNav(list, null))).not.toContain('plugin:workboard')
  expect(keys(drawNav(list, [plugin('workboard', { offered_on: [] })]))).not.toContain('plugin:workboard')
  expect(keys(drawNav(list, [plugin('workboard', { state: 'removed' })]))).not.toContain('plugin:workboard')
  // Kept in the list all along.
  expect(keys(list)).toContain('plugin:workboard')
  expect(keys(drawNav(list, [plugin('workboard')]))).toContain('plugin:workboard')
})

test('a plugin without an icon falls back to the generic glyph', () => {
  const drawn = drawNav(addPlugin(defaultNav(), 'hora'), [plugin('hora', { icon: false })])
  expect(drawn.at(-1)?.icon).toBeNull()
})

test('the phone bar takes the first four visible entries, and More lists the rest', () => {
  let list = setHidden(defaultNav(), 2, true) // Sessions hidden
  list = addPlugin(list, 'workboard')
  list = moveEntry(list, list.length - 1, -(list.length - 2)) // Workboard second
  const { bar, more } = phoneNav(drawNav(list, [plugin('workboard')]))
  expect(keys(bar)).toEqual(['home', 'plugin:workboard', 'tasks', 'docs'])
  expect(keys(more)).toEqual(['memories', 'plugins', 'usage'])
})

test("a plugin's page is its entry's while it has one, on any channel; Plugins' otherwise", () => {
  const drawn = drawNav(addPlugin(defaultNav(), 'workboard'), [plugin('workboard')])
  expect(activeKey('/plugins/workboard', drawn)).toBe('plugin:workboard')
  expect(activeKey('/plugins/hoja', drawn)).toBe('plugins')
  expect(activeKey('/plugins', drawn)).toBe('plugins')
  expect(activeKey('/settings', drawn)).toBe('settings')
  expect(activeKey('/nodes/enroll', drawn)).toBe('settings')
  expect(activeKey('/sessions/s1', drawn)).toBe('sessions')
  expect(activeKey('/reviews/r1', drawn)).toBe('home')
})
