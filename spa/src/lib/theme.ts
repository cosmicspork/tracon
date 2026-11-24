// The colour scheme is a per-browser choice, not a node setting: a phone and a
// laptop on the same node may reasonably want different ones. Applying it is
// @tracon/ui's applyTheme, which plugin pages use too.

import { applyTheme, type Theme } from '@tracon/ui'

export { applyTheme, type Theme }

export const themes: Theme[] = ['auto', 'light', 'dark']

const KEY = 'tracon-theme'

export function parseTheme(value: string | null | undefined): Theme {
  return value === 'light' || value === 'dark' ? value : 'auto'
}

export function storedTheme(): Theme {
  try {
    return parseTheme(localStorage.getItem(KEY))
  } catch {
    return 'auto'
  }
}

const listeners = new Set<() => void>()

/** Calls `fn` whenever this page's choice changes; returns a function that stops it. */
export function onThemeChange(fn: () => void): () => void {
  listeners.add(fn)
  return () => listeners.delete(fn)
}

export function setTheme(theme: Theme) {
  applyTheme(theme)
  try {
    if (theme === 'auto') localStorage.removeItem(KEY)
    else localStorage.setItem(KEY, theme)
  } catch {
    /* blocked storage: the choice lasts for this page */
  }
  for (const fn of listeners) fn()
}
