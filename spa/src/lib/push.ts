// Subscribing this browser to pushes from the node it is talking to. The
// browser holds the device key; the node gets the public half and the push
// service's URL, and that is all it ever needs.

import { api, ApiError } from './api'

/** What the browser can do here; a phone in Safari's tab is not installed. */
export function supported(): boolean {
  return (
    typeof window !== 'undefined' &&
    'serviceWorker' in navigator &&
    'PushManager' in window &&
    'Notification' in window
  )
}

/** iOS only pushes to an installed web app, and says nothing about it. */
export function needsInstall(): boolean {
  if (typeof navigator === 'undefined') return false
  const ios = /iPhone|iPad|iPod/.test(navigator.userAgent)
  const standalone =
    (navigator as unknown as { standalone?: boolean }).standalone === true ||
    (typeof matchMedia === 'function' && matchMedia('(display-mode: standalone)').matches)
  return ios && !standalone
}

/** The `applicationServerKey` bytes from the node's base64url key. */
export function keyBytes(b64url: string): Uint8Array<ArrayBuffer> {
  const pad = '='.repeat((4 - (b64url.length % 4)) % 4)
  const b64 = (b64url + pad).replace(/-/g, '+').replace(/_/g, '/')
  const raw = atob(b64)
  const out = new Uint8Array(new ArrayBuffer(raw.length))
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i)
  return out
}

/** How long the service worker gets to become ready before enrolling gives
 *  up: `navigator.serviceWorker.ready` never settles when it failed to
 *  install, and a toggle that spins forever explains nothing. */
const READY_MS = 10_000




/** Where enrolling this browser stopped. */
export type EnrollStage = 'unavailable' | 'denied' | 'service-worker' | 'push-service' | 'node'

/** An enrollment that failed, and at which step. Its message is the
 *  sentence to show; nothing after a failed stage ran, and a browser
 *  subscription the node never recorded has been let go again. */
export class EnrollError extends Error {
  constructor(
    public stage: EnrollStage,
    message: string,
  ) {
    super(message)
  }
}

const detail = (cause: unknown) => (cause instanceof Error ? cause.message : String(cause))

/** The sentence for a stage, with what the browser or node said. */
export function stageMessage(stage: EnrollStage, said = ''): string {
  const tail = said ? ` (${said})` : ''
  switch (stage) {
    case 'unavailable':
      return needsInstall()
        ? 'Push is not available in this browser. On iOS, add tracon to the Home Screen first.'
        : 'Push is not available in this browser.'
    case 'denied':
      return 'Notifications are blocked for this site. Allow them in the browser’s site settings, then turn this on again.'
    case 'service-worker':
      return `The service worker that receives pushes did not start, so nothing was registered${tail}. Reload the page and try again.`
    case 'push-service':
      return `The browser’s push service refused the subscription, so nothing was registered${tail}.`
    case 'node':
      return `The node did not record this device, so it is not registered${tail}.`
  }
}

/** What enrolling touches, so each stage can be exercised on its own. */
export interface EnrollEnv {
  supported(): boolean
  permission(): Promise<NotificationPermission>
  ready(): Promise<{ pushManager: Pick<PushManager, 'getSubscription' | 'subscribe'> }>
  key(): Promise<string>
  register(sub: PushSubscriptionJSON): Promise<unknown>
}

function within<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(what)), ms)
    promise.then(
      (value) => {
        clearTimeout(timer)
        resolve(value)
      },
      (cause) => {
        clearTimeout(timer)
        reject(cause)
      },
    )
  })
}

async function registration(): Promise<ServiceWorkerRegistration | null> {
  if (!supported()) return null
  return within(navigator.serviceWorker.ready, READY_MS, 'not ready after 10 s').catch(() => null)
}

/** This browser's subscription, if it has one. */
export async function current(): Promise<PushSubscription | null> {
  const reg = await registration()
  if (!reg) return null
  return reg.pushManager.getSubscription()
}

const browser: EnrollEnv = {
  supported,
  permission: () => Notification.requestPermission(),
  ready: () => within(navigator.serviceWorker.ready, READY_MS, 'not ready after 10 s'),
  key: async () => (await api.pushKey()).key,
  register: (sub) => api.putPushSubscription(sub),
}

/** Ask, subscribe, register, failing with the stage that stopped it. */
export async function enroll(env: EnrollEnv): Promise<void> {
  if (!env.supported()) throw new EnrollError('unavailable', stageMessage('unavailable'))
  let permission: NotificationPermission
  try {
    permission = await env.permission()
  } catch (cause) {
    throw new EnrollError('denied', stageMessage('denied', detail(cause)))
  }
  if (permission !== 'granted') throw new EnrollError('denied', stageMessage('denied'))
  let reg: Awaited<ReturnType<EnrollEnv['ready']>>
  try {
    reg = await env.ready()
  } catch (cause) {
    throw new EnrollError('service-worker', stageMessage('service-worker', detail(cause)))
  }
  let key: string
  try {
    key = await env.key()
  } catch (cause) {
    throw new EnrollError('node', stageMessage('node', nodeSaid(cause)))
  }
  let sub: PushSubscription
  try {
    sub =
      (await reg.pushManager.getSubscription()) ??
      (await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(key) }))
  } catch (cause) {
    // Chrome reports a permission revoked mid-flow from subscribe itself.
    if (cause instanceof Error && cause.name === 'NotAllowedError') {
      throw new EnrollError('denied', stageMessage('denied'))
    }
    throw new EnrollError('push-service', stageMessage('push-service', detail(cause)))
  }
  try {
    await env.register(sub.toJSON())
  } catch (cause) {
    // A subscription the node never recorded would read as registered the
    // next time this page looks; let it go so the toggle tells the truth.
    await sub.unsubscribe().catch(() => false)
    throw new EnrollError('node', stageMessage('node', nodeSaid(cause)))
  }
}

/** A refusal names the node's reason; anything else means it was not reached. */
function nodeSaid(cause: unknown): string {
  if (cause instanceof ApiError) {
    return cause.status >= 500 ? `its storage failed: ${cause.message}` : cause.message
  }
  return `not reached: ${detail(cause)}`
}

/** Ask, subscribe, register. Must run inside a click: the permission prompt
 *  is only shown from a user gesture. */
export async function enable(): Promise<void> {
  await enroll(browser)
}

/** Tell the node first, then let the subscription go. */
export async function disable(): Promise<void> {
  const sub = await current()
  if (!sub) return
  try {
    await api.deletePushSubscriptionByEndpoint(sub.endpoint)
  } finally {
    await sub.unsubscribe()
  }
}

/** After a login the cookie is new; re-register so the device follows it. */
export async function resync(): Promise<void> {
  try {
    const sub = await current()
    if (sub) await api.putPushSubscription(sub.toJSON())
  } catch {
    // Best effort: the toggle shows the truth next time it is looked at.
  }
}
