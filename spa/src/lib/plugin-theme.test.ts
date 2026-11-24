import { expect, test } from 'bun:test'
import { bridgeFrameTheme, currentTheme, resolveTheme, themedFrameUrl, type BridgeEnv } from './plugin-theme'
import type { Theme } from './theme'

const URL_ = 'http://127.0.0.1:7441/t/abc/index.html'

test('the theme is resolved, never auto: the choice, or the system scheme under auto', () => {
  expect(resolveTheme('light', false)).toBe('light')
  expect(resolveTheme('dark', true)).toBe('dark')
  expect(resolveTheme('auto', true)).toBe('light')
  expect(resolveTheme('auto', false)).toBe('dark')
})

test("the frame's URL carries the resolved theme and keeps the rest", () => {
  expect(themedFrameUrl(URL_, 'dark')).toBe(`${URL_}?tracon_theme=dark`)
  expect(themedFrameUrl(`${URL_}?a=1#top`, 'light')).toBe(`${URL_}?a=1&tracon_theme=light#top`)
  expect(themedFrameUrl(`${URL_}?tracon_theme=dark`, 'light')).toBe(`${URL_}?tracon_theme=light`)
})

// A frame, a system scheme and a stored choice that the test drives.
const rig = (stored: Theme = 'auto', systemLight = false) => {
  const posts: { message: unknown; origin: string }[] = []
  const frameListeners = new Map<string, Set<() => void>>()
  const mediaListeners = new Set<() => void>()
  const subscribers = new Set<() => void>()
  const state = { stored, systemLight }
  const frame = {
    contentWindow: { postMessage: (message: unknown, origin: string) => void posts.push({ message, origin }) },
    addEventListener: (type: string, fn: () => void) => {
      if (!frameListeners.has(type)) frameListeners.set(type, new Set())
      frameListeners.get(type)!.add(fn)
    },
    removeEventListener: (type: string, fn: () => void) => void frameListeners.get(type)?.delete(fn),
  } as unknown as Parameters<typeof bridgeFrameTheme>[0]
  const env: BridgeEnv = {
    stored: () => state.stored,
    media: {
      get matches() {
        return state.systemLight
      },
      addEventListener: (_: string, fn: () => void) => void mediaListeners.add(fn),
      removeEventListener: (_: string, fn: () => void) => void mediaListeners.delete(fn),
    } as unknown as BridgeEnv['media'],
    subscribe: (fn) => {
      subscribers.add(fn)
      return () => void subscribers.delete(fn)
    },
  }
  return {
    posts,
    env,
    frame,
    load: () => frameListeners.get('load')?.forEach((fn) => fn()),
    choose: (t: Theme) => {
      state.stored = t
      subscribers.forEach((fn) => fn())
    },
    system: (light: boolean) => {
      state.systemLight = light
      mediaListeners.forEach((fn) => fn())
    },
    listening: () =>
      [...frameListeners.values()].reduce((n, s) => n + s.size, 0) + mediaListeners.size + subscribers.size,
  }
}

test('it posts the theme on load, to the frame origin and never *', () => {
  const r = rig('auto', true)
  bridgeFrameTheme(r.frame, URL_, r.env)
  expect(r.posts).toEqual([])
  r.load()
  expect(r.posts).toEqual([{ message: { type: 'tracon/theme', theme: 'light' }, origin: 'http://127.0.0.1:7441' }])
})

test('it posts again when the stored theme changes', () => {
  const r = rig('dark')
  bridgeFrameTheme(r.frame, URL_, r.env)
  r.choose('light')
  r.choose('auto')
  expect(r.posts.map((p) => (p.message as { theme: string }).theme)).toEqual(['light', 'dark'])
  expect(r.posts.every((p) => p.origin === 'http://127.0.0.1:7441')).toBe(true)
})

test('a system change posts under auto and not under an explicit choice', () => {
  const r = rig('auto', false)
  bridgeFrameTheme(r.frame, URL_, r.env)
  r.system(true)
  expect(r.posts.map((p) => (p.message as { theme: string }).theme)).toEqual(['light'])
  r.choose('dark')
  r.system(false)
  r.system(true)
  expect(r.posts.map((p) => (p.message as { theme: string }).theme)).toEqual(['light', 'dark'])
  expect(currentTheme(r.env)).toBe('dark')
})

test('stopping the bridge removes every listener', () => {
  const r = rig()
  const stop = bridgeFrameTheme(r.frame, URL_, r.env)
  expect(r.listening()).toBe(3)
  stop()
  expect(r.listening()).toBe(0)
  r.load()
  r.choose('light')
  expect(r.posts).toEqual([])
})
