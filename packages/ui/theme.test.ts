import { expect, test } from 'bun:test'
import { applyTheme, connectTheme, THEME_MESSAGE } from './theme'

// A root element that records only what applyTheme touches.
const fakeRoot = (initial?: string) => {
  const attrs = new Map<string, string>()
  if (initial) attrs.set('data-theme', initial)
  return {
    attrs,
    setAttribute: (k: string, v: string) => void attrs.set(k, v),
    removeAttribute: (k: string) => void attrs.delete(k),
  } as unknown as HTMLElement & { attrs: Map<string, string> }
}

// A window with a location, a parent and message listeners; `framed: false`
// makes it its own parent, as a top-level page is.
const fakeWindow = (search = '', framed = true) => {
  const listeners = new Set<(e: MessageEvent) => void>()
  const parent = {} as Window
  const win = {
    location: { search },
    addEventListener: (type: string, fn: (e: MessageEvent) => void) => {
      if (type === 'message') listeners.add(fn)
    },
    removeEventListener: (type: string, fn: (e: MessageEvent) => void) => {
      if (type === 'message') listeners.delete(fn)
    },
    post(data: unknown, source: unknown = win.parent) {
      for (const fn of listeners) fn({ data, source } as MessageEvent)
    },
    listeners,
  } as unknown as Window & {
    post: (data: unknown, source?: unknown) => void
    listeners: Set<unknown>
  }
  ;(win as { parent: Window }).parent = framed ? parent : win
  return win
}

test('applyTheme sets data-theme for light and dark and removes it for auto', () => {
  const root = fakeRoot()
  applyTheme('light', root)
  expect(root.attrs.get('data-theme')).toBe('light')
  applyTheme('dark', root)
  expect(root.attrs.get('data-theme')).toBe('dark')
  applyTheme('auto', root)
  expect(root.attrs.has('data-theme')).toBe(false)
})

test('connectTheme applies the tracon_theme query parameter before any message', () => {
  const root = fakeRoot()
  connectTheme(fakeWindow('?x=1&tracon_theme=dark'), root)
  expect(root.attrs.get('data-theme')).toBe('dark')

  const other = fakeRoot()
  connectTheme(fakeWindow('?tracon_theme=auto'), other)
  expect(other.attrs.has('data-theme')).toBe(false)
})

test('connectTheme applies a theme message from the parent window', () => {
  const root = fakeRoot()
  const win = fakeWindow('?tracon_theme=dark')
  connectTheme(win, root)
  win.post({ type: THEME_MESSAGE, theme: 'light' })
  expect(root.attrs.get('data-theme')).toBe('light')
  win.post({ type: 'tracon/theme', theme: 'dark' })
  expect(root.attrs.get('data-theme')).toBe('dark')
})

test('connectTheme ignores messages from another source, of another type or with no theme', () => {
  const root = fakeRoot()
  const win = fakeWindow('?tracon_theme=dark')
  connectTheme(win, root)
  win.post({ type: THEME_MESSAGE, theme: 'light' }, {})
  win.post({ type: THEME_MESSAGE, theme: 'light' }, null)
  win.post({ type: 'tracon/other', theme: 'light' })
  win.post({ type: THEME_MESSAGE, theme: 'sepia' })
  win.post({ type: THEME_MESSAGE, theme: 'auto' })
  win.post('tracon/theme')
  win.post(null)
  expect(root.attrs.get('data-theme')).toBe('dark')
})

test('connectTheme opened standalone changes nothing, and the system setting stands', () => {
  const root = fakeRoot()
  const win = fakeWindow('', false)
  connectTheme(win, root)
  expect(root.attrs.has('data-theme')).toBe(false)
  // A top-level page is its own parent: nothing it receives is the interface.
  win.post({ type: THEME_MESSAGE, theme: 'dark' }, win)
  expect(root.attrs.has('data-theme')).toBe(false)
})

test('the function connectTheme returns stops listening', () => {
  const root = fakeRoot()
  const win = fakeWindow()
  const disconnect = connectTheme(win, root)
  expect(win.listeners.size).toBe(1)
  disconnect()
  expect(win.listeners.size).toBe(0)
  win.post({ type: THEME_MESSAGE, theme: 'light' })
  expect(root.attrs.has('data-theme')).toBe(false)
})
