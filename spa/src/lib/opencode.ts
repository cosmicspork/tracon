// The in-scope shell for OpenCode's own interface.
//
// The native view lives on an origin of its own (`node/src/http/ui.rs`), which
// is the whole reason it can be embedded safely: it holds none of this page's
// authority, and this page holds none of its capability. What is left is a
// handful of rules about the URL that joins them, and they are here rather
// than in the component because each one is a thing to assert.
//
// The capability is in the *fragment*. A fragment is not sent to a server, not
// written to an access log, and not carried in a `Referer` — which is only
// true for as long as nothing on this side copies it somewhere that is. So the
// minted URL goes into the iframe's `src` and nowhere else: not into
// `history`, not into an error message, not into the console. `redact` is what
// every path that reports a boot URL must go through.

import { api } from './api'

/** What `POST /api/sessions/{id}/opencode-boot` answers. */
export interface BootUrl {
  /** The UI origin, with the single-use capability in its fragment. */
  url: string
  /** The origin that URL must be on, as the node computed it. */
  origin: string
  /** When the capability in the fragment stops being worth anything. */
  expires_ms: number
  /** How long the cookie it is exchanged for lives, so the shell knows when a
      backgrounded frame has certainly gone stale. */
  cookie_ttl_ms?: number
}

/** The shell route for a session. In `scope` of the installed app by
    construction: it is a path under the manifest's `/`, not a new origin. */
export function opencodeShellPath(sessionId: string): string {
  return `/sessions/${encodeURIComponent(sessionId)}/opencode`
}

/** The session a shell path names, or null if the path is not one. */
export function shellSessionId(path: string): string | null {
  const m = /^\/sessions\/([^/]+)\/opencode\/?$/.exec(path)
  if (!m) return null
  try {
    return decodeURIComponent(m[1])
  } catch {
    return m[1]
  }
}

/** Whether a path is inside an installed app's scope. The manifest claims
    `/`, so this is every same-origin path — stated as a function because the
    shell route's whole point is that it stays true. */
export function inScope(path: string, scope = '/'): boolean {
  return path.startsWith(scope)
}

/** A boot URL with its capability removed, for anything that is written down.
    Returns the origin and path only — never the query, never the fragment. */
export function redact(url: string): string {
  try {
    const u = new URL(url)
    return `${u.origin}${u.pathname}`
  } catch {
    return '<not a URL>'
  }
}

export class BootUrlRefused extends Error {}

/**
 * Check a minted boot URL before it is handed to an iframe, and answer with
 * the exact string to use as `src`.
 *
 * The rules are the wrapper's (`wrapper/src/opencode.rs`), restated for the
 * browser because the browser is a second caller of the same mint route and a
 * node that answered differently must be refused here too:
 *
 * * `http(s)` only — nothing else may be framed;
 * * the origin the node named, exactly, so a URL that drifted from the
 *   configured origin never loads;
 * * **not** this origin — the isolation is the origin, so a boot URL sharing
 *   tracon's own would put the native view inside this page's authority;
 * * the capability in the fragment and not in the query, which is what keeps
 *   it out of the node's access log.
 */
export function frameSrc(minted: BootUrl, selfOrigin: string): string {
  let u: URL
  try {
    u = new URL(minted.url)
  } catch {
    throw new BootUrlRefused('the node did not answer with a URL')
  }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') {
    throw new BootUrlRefused(`this node offered a ${u.protocol} boot URL, which is not framed`)
  }
  if (u.origin !== minted.origin) {
    throw new BootUrlRefused(
      `this node's boot URL is on ${u.origin}, not the ${minted.origin} it named`,
    )
  }
  if (u.origin === selfOrigin) {
    throw new BootUrlRefused(
      'this node serves OpenCode on its own origin; a boot URL sharing this one is refused',
    )
  }
  if (new URLSearchParams(u.search).has('boot')) {
    throw new BootUrlRefused('this node put the capability in the query, where it would be logged')
  }
  const hash = u.hash.startsWith('#') ? u.hash.slice(1) : u.hash
  const q = hash.indexOf('?')
  const carried = q >= 0 && new URLSearchParams(hash.slice(q + 1)).has('boot')
  if (!carried) {
    throw new BootUrlRefused('this node minted a boot URL with no capability in it')
  }
  return u.href
}

/** Mint a capability for this session. The caller is the operator already —
    the route is behind the operator guard — and what comes back is single-use
    and short-lived. */
export function mint(sessionId: string): Promise<BootUrl> {
  return api.opencodeBoot(sessionId)
}

/** The node's cookie lifetime, when it told us; twelve hours is what
    `ui::COOKIE_TTL_MS` has been since #206, and an older node that does not
    say is assumed to agree. */
export const DEFAULT_COOKIE_TTL_MS = 12 * 60 * 60 * 1000

/**
 * When an embedded view certainly stopped working, given when it booted.
 *
 * "Certainly" is the operative word: the cookie is `HttpOnly`, so this side
 * cannot read it, and the shell must not guess that a frame is fine. It can
 * only know when one is definitely finished — after which the honest move is
 * to offer a reconnect rather than leave a dead frame on screen.
 */
export function staleAfter(bootedMs: number, cookieTtlMs?: number): number {
  return bootedMs + (cookieTtlMs && cookieTtlMs > 0 ? cookieTtlMs : DEFAULT_COOKIE_TTL_MS)
}
