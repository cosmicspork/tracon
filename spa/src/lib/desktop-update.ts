import { invoke, isTauri } from '@tauri-apps/api/core'

export type UpdateState =
  | 'unsupported'
  | 'idle'
  | 'checking'
  | 'current'
  | 'available'
  | 'downloading'
  | 'failed'

export interface UpdateStatus {
  state: UpdateState
  current_version: string
  available_version?: string
  message?: string
}

export type DesktopUpdateAction =
  | { command: 'check' | 'install'; label: string; disabled: false }
  | { command: null; label: string; disabled: true }
  | null


export function desktopUpdatesAvailable(): boolean {
  return (
    typeof window !== 'undefined' &&
    isTauri() &&
    window.location.protocol === 'http:' &&
    (window.location.hostname === '127.0.0.1' || window.location.hostname === 'localhost')
  )
}

export async function status(): Promise<UpdateStatus | null> {
  if (!desktopUpdatesAvailable()) return null
  return invoke<UpdateStatus>('desktop_update_status')
}

export async function check(): Promise<UpdateStatus | null> {
  if (!desktopUpdatesAvailable()) return null
  return invoke<UpdateStatus>('desktop_check_for_update')
}

export async function install(): Promise<UpdateStatus | null> {
  if (!desktopUpdatesAvailable()) return null
  return invoke<UpdateStatus>('desktop_install_update')
}

export function desktopUpdateAction(status: UpdateStatus): DesktopUpdateAction {
  switch (status.state) {
    case 'idle':
      return { command: 'check', label: 'Check for updates', disabled: false }
    case 'current':
      return { command: 'check', label: 'Check again', disabled: false }
    case 'failed':
      return { command: 'check', label: 'Try again', disabled: false }
    case 'available':
      return {
        command: 'install',
        label: `Update to v${status.available_version} and restart`,
        disabled: false,
      }
    case 'checking':
      return { command: null, label: 'Checking…', disabled: true }
    case 'downloading':
      return { command: null, label: 'Installing…', disabled: true }
    case 'unsupported':
      return null
  }
}
