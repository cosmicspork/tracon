// Pure helpers for the settings pane: minting an operator token and the
// address that carries it. What a config form changed is `config-form.ts`.

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
