import { invoke, isTauri } from '@tauri-apps/api/core'

/**
 * OpenCode's own interface, in a second desktop window that holds none of this
 * one's privileges.
 *
 * The window is the desktop app's to open: it is a native window with its own
 * capability, navigable only to the node's OpenCode UI origin, and a browser
 * tab has no equivalent to offer. Everything about the boundary lives in the
 * wrapper (`wrapper/src/opencode.rs`); this is the button.
 */
export function desktopCanOpenOpencode(): boolean {
  return typeof window !== 'undefined' && isTauri()
}

/**
 * Ask the desktop app to open this session's OpenCode window. The app asks the
 * node for a boot URL — the token in it never passes through this page — and
 * refuses anything that is not the UI origin. A node that does not serve that
 * origin rejects with a sentence saying so, which is the caller's to show.
 */
export function openOpencodeWindow(sessionId: string): Promise<void> {
  return invoke<void>('desktop_open_opencode', { sessionId })
}
