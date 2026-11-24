// The colour scheme for a page built on @tracon/ui: the interface itself, or a
// plugin page framed by it. tokens.css reads `data-theme` on the root element;
// with none, the page follows the system setting.

export type Theme = 'auto' | 'light' | 'dark'

/** The `type` of the message the interface posts to a framed plugin page. */
export const THEME_MESSAGE = 'tracon/theme'

/** The query parameter carrying the interface's theme into a framed page. */
export const THEME_PARAM = 'tracon_theme'

/** What the interface posts to a plugin frame: always resolved, never `auto`. */
export type ThemeMessage = { type: typeof THEME_MESSAGE; theme: 'light' | 'dark' }

/** Sets `data-theme` for an explicit choice, or removes it to follow the system. */
export function applyTheme(theme: Theme, root: HTMLElement = document.documentElement): void {
  if (theme === 'auto') root.removeAttribute('data-theme')
  else root.setAttribute('data-theme', theme)
}

const resolved = (value: unknown): 'light' | 'dark' | null =>
  value === 'light' || value === 'dark' ? value : null

/**
 * Follows the theme of the Tracon interface framing this page. Call it once,
 * as early as possible (top of `main.ts`): it applies the `tracon_theme` query
 * parameter at once, so the first paint is already right, then applies every
 * `{ type: "tracon/theme", theme }` message posted by the parent window.
 * Messages from anywhere else, or of another shape, are ignored.
 *
 * Opened outside a frame, with no parameter and no message, it changes
 * nothing, so the page follows the system setting. Returns a function that
 * stops listening.
 */
export function connectTheme(
  win: Window = window,
  root: HTMLElement = win.document.documentElement,
): () => void {
  let initial: 'light' | 'dark' | null = null
  try {
    initial = resolved(new URLSearchParams(win.location.search).get(THEME_PARAM))
  } catch {
    /* no readable location: wait for a message */
  }
  if (initial) applyTheme(initial, root)

  const onMessage = (event: MessageEvent) => {
    if (win.parent === win || event.source !== win.parent) return
    const data = event.data as Partial<ThemeMessage> | null
    if (!data || typeof data !== 'object' || data.type !== THEME_MESSAGE) return
    const theme = resolved(data.theme)
    if (theme) applyTheme(theme, root)
  }
  win.addEventListener('message', onMessage)
  return () => win.removeEventListener('message', onMessage)
}
