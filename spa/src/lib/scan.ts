// What a scanned QR code is to tracon. The scanner (`QrScanner.svelte`) reads
// text off the camera; this decides whether it is an address tracon itself
// prints, and nothing else. Every QR tracon makes is an origin with a bare
// `/` path and the payload in the fragment: a device approval
// (`approve_url`), a login (`loginUrl()`) or a mesh invitation
// (`invite_url()`). Anything else is not acted on, and a result is never
// navigated to: a caller opens a view for it, it does not follow it.

import { tokenFromHash } from './auth'
import { approvalFromHash } from './device'

export type ScanResult =
  | { kind: 'approve'; id: string }
  | { kind: 'login'; token: string }
  | { kind: 'enroll' }
  | { kind: 'elsewhere'; origin: string }
  | { kind: 'unknown' }

/** The kinds a scanner can be asked to accept. */
export type ScanKind = 'approve' | 'login' | 'enroll'

/** Longer than any address tracon prints, and than the node will encode. */
export const MAX_SCAN = 2048

const ENROLL = /^#enroll=[A-Za-z0-9-]+$/

/** Classify scanned text against the origin this page runs on. */
export function classifyScan(text: string, here: { origin: string }): ScanResult {
  const t = text.trim()
  if (!t || t.length > MAX_SCAN) return { kind: 'unknown' }
  let url: URL
  try {
    url = new URL(t)
  } catch {
    return { kind: 'unknown' }
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') return { kind: 'unknown' }
  if (url.username || url.password) return { kind: 'unknown' }
  if (url.pathname !== '/' || url.search !== '') return { kind: 'unknown' }

  // A mesh invitation names the hub's origin, not this one's.
  if (ENROLL.test(url.hash)) return { kind: 'enroll' }

  const id = approvalFromHash(url.hash)
  const token = id ? null : tokenFromHash(url.hash)
  if (!id && !token) return { kind: 'unknown' }
  // The session cookie and the passkey's relying party belong to this
  // origin: an approval or a login for another one cannot be used here.
  if (url.origin !== here.origin) return { kind: 'elsewhere', origin: url.origin }
  return id ? { kind: 'approve', id } : { kind: 'login', token: token! }
}

/** What the scanner says about a code it will not hand on. Plain text: the
    origin is shown, never linked, and unknown content is never echoed. */
export function scanRefusal(result: ScanResult): string {
  switch (result.kind) {
    case 'elsewhere':
      return `This code is for ${result.origin}. Open tracon there to approve it.`
    case 'enroll':
      return 'This is a mesh invitation. Join it from Settings > Mesh on the node that should join.'
    case 'login':
      return "That's a sign-in code. This app is already signed in."
    case 'approve':
      return 'Sign in first, then scan the code again.'
    default:
      return "That QR code isn't a tracon code."
  }
}

/** Whether this window offers a scanner at all. The desktop app is the window
    that asks, not the one that approves; a camera needs a secure context. */
export function canScan(env: { tauri: boolean; secure: boolean; camera: boolean }): boolean {
  return !env.tauri && env.secure && env.camera
}

/** The scanner's environment, read from the window. */
export function scanEnvironment(tauri: boolean): { tauri: boolean; secure: boolean; camera: boolean } {
  if (typeof window === 'undefined') return { tauri, secure: false, camera: false }
  return {
    tauri,
    secure: window.isSecureContext === true,
    camera: typeof navigator.mediaDevices?.getUserMedia === 'function',
  }
}

/** Why the camera did not open, in words, by the `DOMException` name. */
export function cameraProblem(err: unknown): string {
  const name = err instanceof Error || (typeof err === 'object' && err !== null && 'name' in err) ? (err as { name: string }).name : ''
  switch (name) {
    case 'NotAllowedError':
    case 'SecurityError':
      return 'Camera access is blocked. Allow the camera for this site in the browser’s site settings, and for the browser or app in the system settings, then try again.'
    case 'NotFoundError':
    case 'OverconstrainedError':
      return 'No camera was found on this device.'
    case 'NotReadableError':
    case 'AbortError':
      return 'The camera is in use by another app. Close it there and try again.'
    default:
      return 'The camera could not be opened.'
  }
}
