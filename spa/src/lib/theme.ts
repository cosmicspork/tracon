// The colour scheme is a per-browser choice, not a node setting: a phone and a
// laptop on the same node may reasonably want different ones.

export type Theme = 'auto' | 'light' | 'dark'

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

export function applyTheme(theme: Theme, root: HTMLElement = document.documentElement) {
  if (theme === 'auto') root.removeAttribute('data-theme')
  else root.setAttribute('data-theme', theme)
}

export function setTheme(theme: Theme) {
  applyTheme(theme)
  try {
    if (theme === 'auto') localStorage.removeItem(KEY)
    else localStorage.setItem(KEY, theme)
  } catch {
    /* blocked storage: the choice lasts for this page */
  }
}
