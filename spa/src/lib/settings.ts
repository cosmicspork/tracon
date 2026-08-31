// Pure helpers for the settings pane: minting an operator token, and working
// out what a config form actually changed.

/**
 * Mint an operator token the way `tracon auth issue` does — 32 random bytes,
 * base64url, behind a `trc1.` marker. Minted here rather than at the node so
 * the node only ever learns the hash.
 */
export function mintToken(bytes = crypto.getRandomValues(new Uint8Array(32))): string {
  let binary = ''
  for (const b of bytes) binary += String.fromCharCode(b)
  const b64 = btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
  return `trc1.${b64}`
}

/** SHA-256, lowercase hex: what the node stores, and all it ever stores. */
export async function hashToken(token: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(token))
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('')
}

/** The login URL a QR encodes: the token rides the fragment, never the path. */
export function loginUrl(base: string, token: string): string {
  return `${base.replace(/\/+$/, '')}/#token=${token}`
}

type Json = string | number | boolean | null | Json[] | { [k: string]: Json }

/**
 * The subset of `edited` that differs from `original`, shaped as the node's
 * patch: only what changed, nested sections included only when something
 * inside them did. Sending the whole form back would report a restart owed
 * for settings nobody touched.
 */
export function changedSubset(
  original: Record<string, Json>,
  edited: Record<string, Json>,
): Record<string, Json> {
  const patch: Record<string, Json> = {}
  for (const [key, next] of Object.entries(edited)) {
    const before = original[key]
    if (isObject(next) && isObject(before)) {
      const inner = changedSubset(before, next)
      if (Object.keys(inner).length > 0) patch[key] = inner
    } else if (JSON.stringify(before) !== JSON.stringify(next)) {
      patch[key] = next
    }
  }
  return patch
}

function isObject(v: unknown): v is Record<string, Json> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}
