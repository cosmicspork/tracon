// Device approval: a window that cannot make a passkey (the desktop app
// against a remote node) signs in, or confirms an admin action, by having a
// browser that is signed in with one approve it. The node does the deciding;
// this module only reads the approval address, polls for the outcome the way
// RFC 8628 says to, and words what each side shows.

import { ApiError, type StepUp } from './api'
import { EXPIRY_MARGIN_MS } from './passkey'

/** What the node hands a window that asked. `device_code` is the secret the
    window polls with; it never goes in a URL or leaves this window. */
export interface DeviceStart {
  approval_id: string
  kind: 'sign-in' | 'step-up'
  device_code: string
  /** `BCDF-GHJK`: the code both screens show, to be compared. */
  user_code: string
  /** `<public origin>/#approve=<id>`: opened in a browser that is signed in. */
  approve_url: string
  /** The same address as a QR (SVG, made by the node), for a phone. */
  approve_qr?: string
  /** Seconds between polls, to begin with. */
  interval: number
  expires_in: number
  expires_ms: number
  op_hash?: string
  action?: string | null
  summary?: string | null
}

/** What a collected step-up hands back: a proof for the original request. */
export interface DeviceProof {
  kind: 'step-up'
  proof: string
  op_hash: string
  expires_ms: number
}

/** What the approving browser is shown. */
export interface DeviceApprovalView {
  id: string
  kind: 'sign-in' | 'step-up'
  user_code: string
  /** `pending`, `approved`, `denied`, `consumed`, `revoked` or `expired`. */
  status: string
  created_ms: number
  expires_ms: number
  requester_agent: string | null
  requester_addr: string | null
  action: string | null
  summary: string | null
  /** The line the passkey prompt will show. */
  prompt: string
  /** This browser is at the node: approving needs no passkey. */
  local: boolean
  passkeys: number
  relying_party: boolean
  /** This browser is the session that asked. */
  same_session: boolean
}

/** The approval an address names, from its fragment (`#approve=<id>`). The
    ID is not a secret (approving takes a passkey), but it stays out of
    request lines all the same. */
export function approvalFromHash(hash: string): string | null {
  const m = hash.match(/^#approve=([A-Za-z0-9_-]{16,64})$/)
  return m ? m[1] : null
}

let stashed: string | null = null

/** Remember the approval the page was opened for. */
export function stashApproval(id: string | null): void {
  stashed = id
}

export function stashedApproval(): string | null {
  return stashed
}

/** Whether this window confirms by device approval rather than its own
    passkey: the desktop app's webview has none to offer on Linux or in an
    unsigned macOS build, and a browser without WebAuthn has none at all. */
export function confirmsElsewhere(env: { tauri: boolean; passkeys: boolean }): boolean {
  return env.tauri || !env.passkeys
}

export class DeviceDenied extends Error {
  constructor() {
    super('The request was denied in the other browser.')
    this.name = 'DeviceDenied'
  }
}

export class DeviceExpired extends Error {
  constructor() {
    super('The request expired before it was approved. Ask again.')
    this.name = 'DeviceExpired'
  }
}

export class DeviceCancelled extends Error {
  constructor() {
    super('Cancelled.')
    this.name = 'DeviceCancelled'
  }
}

/** RFC 8628: a window told to slow down adds five seconds for good. */
export const SLOW_DOWN_STEP_MS = 5_000

function wait(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(new DeviceCancelled())
    const t = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort)
      resolve()
    }, ms)
    const onAbort = () => {
      clearTimeout(t)
      reject(new DeviceCancelled())
    }
    signal?.addEventListener('abort', onAbort, { once: true })
  })
}

export interface PollOptions {
  sleep?: (ms: number, signal?: AbortSignal) => Promise<void>
  now?: () => number
  signal?: AbortSignal
}

/** Poll until the request is approved, then return what collecting it gave.
    Waits the interval before every poll, adds five seconds whenever told to
    slow down (or rate-limited), and keeps trying through a dropped
    connection until the request would have expired. */
export async function pollForApproval<T>(
  start: Pick<DeviceStart, 'device_code' | 'interval' | 'expires_ms'>,
  poll: (deviceCode: string) => Promise<T>,
  options: PollOptions = {},
): Promise<T> {
  const sleep = options.sleep ?? wait
  const now = options.now ?? Date.now
  const signal = options.signal
  let interval = Math.max(1, start.interval || 5) * 1000
  for (;;) {
    await sleep(interval, signal)
    if (signal?.aborted) throw new DeviceCancelled()
    try {
      return await poll(start.device_code)
    } catch (err) {
      if (signal?.aborted) throw new DeviceCancelled()
      if (err instanceof ApiError) {
        if (err.kind === 'authorization_pending') continue
        if (err.kind === 'slow_down' || err.status === 429) {
          interval += SLOW_DOWN_STEP_MS
          continue
        }
        if (err.kind === 'access_denied') throw new DeviceDenied()
        if (err.kind === 'expired_token' || err.kind === 'invalid_grant') throw new DeviceExpired()
        // Anything else the node refused will not change by asking again.
        if (err.status >= 400 && err.status < 500) throw err
      }
      // The node did not answer: try again until the request is gone.
      if (now() >= start.expires_ms) throw new DeviceExpired()
    }
  }
}

/** What handing a step-up to a device approval needs from the node. */
export interface HandOverCalls {
  /** A fresh challenge for the same operation (`/api/passkeys/assert/options`). */
  assertOptions: (opHash: string) => Promise<Pick<StepUp, 'challenge_id' | 'expires_ms'>>
  /** The device approval for a challenge (`/api/auth/device/stepup`). */
  deviceStepUp: (challengeId: string) => Promise<DeviceStart>
}

/** Hand an admin action's challenge to a device approval, which spends it.
    A challenge that has expired, or that the node says is gone, is replaced
    with a fresh one for the same operation first; the node records what the
    operation is, so the approving browser sees the node's words for it. */
export async function handOver(stepUp: StepUp, calls: HandOverCalls, now = Date.now()): Promise<DeviceStart> {
  let challenge = stepUp.challenge_id
  if (now > stepUp.expires_ms - EXPIRY_MARGIN_MS) {
    challenge = (await calls.assertOptions(stepUp.op_hash)).challenge_id
  }
  try {
    return await calls.deviceStepUp(challenge)
  } catch (err) {
    if (!(err instanceof ApiError && err.status === 410)) throw err
    return calls.deviceStepUp((await calls.assertOptions(stepUp.op_hash)).challenge_id)
  }
}

/** What the approving browser says about the request, to compare before
    approving. */
export function approvalHeadline(v: Pick<DeviceApprovalView, 'kind' | 'summary' | 'action'>): string {
  if (v.kind === 'sign-in') return 'Sign in another window?'
  const what = v.summary?.trim() || v.action?.trim() || 'an admin action'
  return `Confirm for another window: ${what}`
}

/** Why the approving browser cannot approve here, or null when it can. */
export function cannotApprove(
  v: Pick<DeviceApprovalView, 'status' | 'local' | 'passkeys' | 'relying_party'>,
  passkeys: boolean,
): string | null {
  if (v.status !== 'pending') return null
  if (v.local) return null
  if (!v.relying_party) return 'This node has no public origin set, so passkeys cannot be used with it.'
  if (v.passkeys === 0)
    return 'No passkey is enrolled on this node yet. Approve this at the node itself, or enroll one first.'
  if (!passkeys) return 'This browser cannot use passkeys. Open this link on a device that can, such as your phone.'
  return null
}

/** The word for a request that is no longer waiting. */
export function settled(status: string): string | null {
  switch (status) {
    case 'pending':
      return null
    case 'approved':
    case 'consumed':
      return 'Approved. Return to the window that asked.'
    case 'denied':
      return 'Denied. The window that asked was told.'
    case 'expired':
      return 'This request expired. Ask again from the window.'
    case 'revoked':
      return 'This request was cancelled when the node’s sign-ins or passkeys changed.'
    default:
      return `This request is ${status}.`
  }
}

/** What to tell the operator when a device approval did not come through. */
export function deviceProblem(err: unknown): string {
  if (err instanceof DeviceDenied || err instanceof DeviceExpired) return err.message
  return err instanceof Error ? err.message : String(err)
}
