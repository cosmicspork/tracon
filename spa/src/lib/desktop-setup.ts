import { invoke, isTauri } from '@tauri-apps/api/core'

export interface SetupStatus {
  platform: 'macos' | 'linux'
  owner: 'none' | 'service' | 'migrated' | 'foreign'
  node_version: string | null
  sidecar_version: string | null
  cli_version: string | null
  cli_path: string | null
  path_hint: string | null
  service_installed: boolean
  service_running: boolean
  podman: string | null
  machine: 'running' | 'starting' | 'stopped' | 'missing' | null
}

export interface SetupStep {
  id: 'podman' | 'machine' | 'service' | 'cli'
  title: string
  detail: string
  command?: string
  done: boolean
  // Nothing after it can work until it is done.
  blocking: boolean
}

function serviceStep(s: SetupStatus): SetupStep {
  const manager = s.platform === 'macos' ? 'launchd' : 'systemd'
  const step = { id: 'service' as const, title: 'Background service', done: false, blocking: false }
  switch (s.owner) {
    case 'service':
      return {
        ...step,
        done: true,
        detail: `The node${s.node_version ? ` v${s.node_version}` : ''} runs under ${manager} and keeps running when this app quits.`,
      }
    case 'foreign':
      return { ...step, detail: 'A node you started yourself is answering. Stop it, then install the service here.' }
    case 'migrated':
      return {
        ...step,
        detail: 'The node is running inside this app, as earlier versions ran it. Installing the service moves it there; running sessions end.',
      }
    case 'none':
      return s.service_installed
        ? { ...step, detail: 'Installed, but the node is not answering.', command: 'tracon service status' }
        : { ...step, detail: `Runs the node under ${manager} at login, whether or not this app is open.` }
  }
}

function cliStep(s: SetupStatus): SetupStep {
  const step = { id: 'cli' as const, title: 'Command line', blocking: false }
  if (s.cli_version === null) {
    return { ...step, done: false, detail: `Installs tracon at ${s.cli_path ?? '~/.local/bin/tracon'} for your terminal and the service.` }
  }
  if (s.sidecar_version && s.cli_version !== s.sidecar_version) {
    return { ...step, done: false, detail: `${s.cli_path} is v${s.cli_version}; this app carries v${s.sidecar_version}.` }
  }
  if (s.path_hint) {
    return { ...step, done: false, detail: 'Installed, but your shell does not look there. Add this to your shell profile:', command: s.path_hint }
  }
  return { ...step, done: true, detail: `v${s.cli_version} at ${s.cli_path}.` }
}

export function setupSteps(s: SetupStatus): SetupStep[] {
  const steps: SetupStep[] = [
    s.podman
      ? { id: 'podman', title: 'Podman', done: true, blocking: true, detail: `Found at ${s.podman}.` }
      : {
          id: 'podman',
          title: 'Podman',
          done: false,
          blocking: true,
          detail: 'The boundary the agent runs inside. Install it, then this page picks it up.',
          command: s.platform === 'macos' ? 'brew install podman' : undefined,
        },
  ]
  if (s.platform === 'macos' && s.podman) {
    steps.push(
      s.machine === 'missing' || s.machine === null
        ? { id: 'machine', title: 'Podman machine', done: false, blocking: true, detail: 'Create one once; the node starts it after that.', command: 'podman machine init' }
        : { id: 'machine', title: 'Podman machine', done: true, blocking: true, detail: s.machine === 'running' ? 'Running.' : 'Not running; the node starts it.' },
    )
  }
  steps.push(serviceStep(s), cliStep(s))
  return steps
}

export function blocked(steps: SetupStep[]): boolean {
  return steps.some((s) => s.blocking && !s.done)
}

export const setupStatus = () => invoke<SetupStatus>('desktop_setup_status')
export const installService = () => invoke<SetupStatus>('desktop_install_service')
export const installCli = () => invoke<SetupStatus>('desktop_install_cli')
export const restartNode = () => invoke<SetupStatus>('desktop_restart_node')
export const openNode = () => invoke<void>('desktop_open_node')
export const inDesktopApp = () => typeof window !== 'undefined' && isTauri()
