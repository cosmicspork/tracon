// Where an address that used to name a screen now leads. Bookmarks, links in
// documents and notifications already delivered keep working after a screen
// moves; the router swaps the address in place rather than adding a step to
// history. Pure, so it tests flat.

/**
 * Settings sections that were folded into others when Settings was regrouped,
 * by the anchor they had. Maintenance was a catch-all; most of it is System.
 */
export const SETTINGS_ALIASES: Record<string, string> = {
  repositories: 'channels',
  devices: 'access',
  data: 'system',
  maintenance: 'system',
}

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
  // A section was only ever a hash, but a path is the obvious thing to type.
  const section = path.match(/^\/settings\/([a-z]+)\/?$/)
  if (section) return `/settings${search}#${SETTINGS_ALIASES[section[1]] ?? section[1]}`
  if (path === '/settings') {
    const moved = SETTINGS_ALIASES[hash.slice(1)]
    if (moved) return `/settings${search}#${moved}`
  }
  return null
}
