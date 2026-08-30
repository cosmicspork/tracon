// The login QR encodes `{node}/#token=trc1.…`. The fragment never leaves the
// browser — it is not sent in requests — and the app strips it from the
// address bar at startup, before login, so a screenshot or a shared link
// cannot re-leak it. The token is stashed here for the login screen to spend.

export function tokenFromHash(hash: string): string | null {
  const m = hash.match(/^#token=([^&]+)$/)
  if (!m) return null
  const t = decodeURIComponent(m[1])
  return t.startsWith('trc1.') ? t : null
}

let stashed: string | null = null

/** Remember a token read from the fragment. */
export function stashToken(t: string): void {
  stashed = t
}

/** The stashed token, once: spending it clears it. */
export function takeToken(): string | null {
  const t = stashed
  stashed = null
  return t
}

/** A Secure cookie will not survive plain http off the machine. */
export function insecureContext(protocol: string, hostname: string): boolean {
  if (protocol === 'https:') return false
  return !['localhost', '127.0.0.1', '[::1]', '::1'].includes(hostname)
}
