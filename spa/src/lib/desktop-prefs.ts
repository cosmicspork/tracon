import { invoke } from '@tauri-apps/api/core'
import { desktopUpdatesAvailable } from './desktop-update'

// How the desktop app itself behaves on this machine. Kept by the app beside
// node.toml, never in it: a phone reading the same node has no dock or ⌘Q.
export interface DesktopPrefs {
  open_window_at_launch: boolean
  cmd_q_quits: boolean
  hide_dock_when_closed: boolean
}

export async function preferences(): Promise<DesktopPrefs | null> {
  if (!desktopUpdatesAvailable()) return null
  return invoke<DesktopPrefs>('desktop_preferences')
}

export async function setPreferences(update: Partial<DesktopPrefs>): Promise<DesktopPrefs> {
  return invoke<DesktopPrefs>('desktop_set_preferences', { update })
}
