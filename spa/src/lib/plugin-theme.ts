// The theme bridge into a plugin's frame. A plugin page is on its own origin
// and cannot see this page's data-theme, so the interface tells it: once in
// the frame's URL (`tracon_theme`, so its first paint is right) and then by
// message, on load and whenever the resolved theme changes. The plugin side
// is connectTheme in @tracon/ui.

import { THEME_MESSAGE, THEME_PARAM, type ThemeMessage } from '@tracon/ui'
import { onThemeChange, storedTheme, type Theme } from './theme'

export type Resolved = 'light' | 'dark'

/** What tokens.css shows for `theme`: dark unless light is chosen, or the system prefers light under auto. */
export function resolveTheme(theme: Theme, systemLight: boolean): Resolved {
  if (theme === 'auto') return systemLight ? 'light' : 'dark'
  return theme
}

/** `url` with `tracon_theme=<theme>` in its query, replacing one already there. */
export function themedFrameUrl(url: string, theme: Resolved): string {
  try {
    const u = new URL(url)
    u.searchParams.set(THEME_PARAM, theme)
    return u.toString()
  } catch {
    return url
  }
}

type MediaLike = Pick<MediaQueryList, 'matches' | 'addEventListener' | 'removeEventListener'>

export type BridgeEnv = {
  stored: () => Theme
  /** `(prefers-color-scheme: light)`, or null where there is no matchMedia. */
  media: MediaLike | null
  subscribe: (fn: () => void) => () => void
}

const browserEnv = (): BridgeEnv => ({
  stored: storedTheme,
  media: typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: light)') : null,
  subscribe: onThemeChange,
})

/** The theme the interface shows now. */
export function currentTheme(env: BridgeEnv = browserEnv()): Resolved {
  return resolveTheme(env.stored(), env.media?.matches ?? false)
}

type FrameLike = Pick<HTMLIFrameElement, 'addEventListener' | 'removeEventListener'> & {
  readonly contentWindow: Pick<Window, 'postMessage'> | null
}

/**
 * Posts the resolved theme to `frame`, to the origin of `url` (never `*`): on
 * the frame's load, when the stored choice changes, and when the system scheme
 * changes while the choice is auto. Returns a function that stops it.
 */
export function bridgeFrameTheme(frame: FrameLike, url: string, env: BridgeEnv = browserEnv()): () => void {
  const origin = new URL(url).origin
  const post = () => {
    const message: ThemeMessage = { type: THEME_MESSAGE, theme: currentTheme(env) }
    try {
      frame.contentWindow?.postMessage(message, origin)
    } catch {
      /* an origin postMessage refuses (an opaque one): nothing to tell */
    }
  }
  const onSystem = () => {
    if (env.stored() === 'auto') post()
  }
  frame.addEventListener('load', post)
  env.media?.addEventListener('change', onSystem)
  const unsubscribe = env.subscribe(post)
  return () => {
    frame.removeEventListener('load', post)
    env.media?.removeEventListener('change', onSystem)
    unsubscribe()
  }
}
