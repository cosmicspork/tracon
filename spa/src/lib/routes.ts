// Where an address that used to name a screen now leads. Bookmarks, links in
// documents and notifications already delivered keep working after a screen
// moves; the router swaps the address in place rather than adding a step to
// history. Pure, so it tests flat.

/**
 * The current address for `path` (with its `search` and `hash`), or null when
 * it is already current.
 */
export function redirect(path: string, search = '', hash = ''): string | null {
  // Usage was Metrics before it had a rail entry of its own.
  if (path === '/metrics') return `/usage${search}${hash}`
  // Work was a group of tabs; its first tab, the task list, is Tasks now.
  if (path === '/work') return `/tasks${search}${hash}`
  const item = path.match(/^\/work\/([^/]+)$/)
  if (item) return `/tasks/${item[1]}${search}${hash}`
  // Nodes left the rail: its facts sit in Settings › Mesh beside the members.
  if (path === '/nodes') return `/settings${search}#mesh`
  return null
}
