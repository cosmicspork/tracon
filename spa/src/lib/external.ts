import { invoke, isTauri } from '@tauri-apps/api/core'
import { openUrl } from '@tauri-apps/plugin-opener'

export interface ReservedWindow {
  opener: unknown
  location: { href: string }
  close(): void
}

export function validateExternalUrl(value: string): string {
  let url: URL
  try {
    url = new URL(value)
  } catch {
    throw new Error('The provider returned an invalid sign-in URL.')
  }
  if (url.protocol !== 'https:' || url.username || url.password) {
    throw new Error('The provider returned an unsafe sign-in URL.')
  }
  return url.toString()
}

export function prepareExternalOpen(): ReservedWindow | null {
  if (typeof window === 'undefined' || isTauri()) return null
  const reserved = window.open('about:blank', '_blank')
  if (!reserved) throw new Error('The browser blocked the sign-in window. Allow pop-ups and try again.')
  reserved.opener = null
  return reserved
}

export async function openExternal(value: string, reserved?: ReservedWindow | null): Promise<void> {
  try {
    const url = validateExternalUrl(value)
    if (isTauri()) {
      await openUrl(url)
      return
    }
    const target = reserved ?? prepareExternalOpen()
    if (!target) throw new Error('A browser window could not be opened for sign-in.')
    target.opener = null
    target.location.href = url
  } catch (error) {
    reserved?.close()
    throw error
  }
}

export async function desktopManagedLocal(): Promise<boolean> {
  if (typeof window === 'undefined' || !isTauri()) return false
  return invoke<boolean>('desktop_managed_local')
}

export function browserCanClaimNode(): boolean {
  if (typeof window === 'undefined' || isTauri()) return false
  return (
    window.location.protocol === 'http:' &&
    (window.location.hostname === 'localhost' ||
      window.location.hostname === '127.0.0.1' ||
      window.location.hostname === '::1')
  )
}
