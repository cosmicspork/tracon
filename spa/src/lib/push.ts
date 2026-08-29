// Subscribing this browser to pushes from the node it is talking to. The
// browser holds the device key; the node gets the public half and the push
// service's URL, and that is all it ever needs.

import { api } from './api'

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

async function registration(): Promise<ServiceWorkerRegistration | null> {
  if (!supported()) return null
  return navigator.serviceWorker.ready
}

/** This browser's subscription, if it has one. */
export async function current(): Promise<PushSubscription | null> {
  const reg = await registration()
  if (!reg) return null
  return reg.pushManager.getSubscription()
}

/** Ask, subscribe, register. Must run inside a click: the permission prompt
 *  is only shown from a user gesture. */
export async function enable(): Promise<void> {
  const reg = await registration()
  if (!reg) throw new Error('push is not available in this browser')
  const permission = await Notification.requestPermission()
  if (permission !== 'granted') throw new Error('notifications were not allowed')
  const { key } = await api.pushKey()
  const sub =
    (await reg.pushManager.getSubscription()) ??
    (await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(key) }))
  await api.putPushSubscription(sub.toJSON())
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
