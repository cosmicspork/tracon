import { expect, test } from 'bun:test'
import { blocked, setupSteps, type SetupStatus } from './desktop-setup'

const fresh: SetupStatus = {
  platform: 'macos',
  owner: 'none',
  node_version: null,
  sidecar_version: '0.15.0',
  cli_version: null,
  cli_path: '/Users/op/.local/bin/tracon',
  path_hint: null,
  service_installed: false,
  service_running: false,
  podman: '/opt/homebrew/bin/podman',
  machine: 'stopped',
}

const byId = (s: SetupStatus) => Object.fromEntries(setupSteps(s).map((x) => [x.id, x]))

test('a fresh mac with podman is asked for the service and the CLI', () => {
  const steps = byId(fresh)
  expect(steps.podman.done).toBe(true)
  // A stopped machine is fine: the node starts it.
  expect(steps.machine.done).toBe(true)
  expect(steps.service.done).toBe(false)
  expect(steps.cli.done).toBe(false)
  expect(blocked(setupSteps(fresh))).toBe(false)
})

test('no podman, or no machine, blocks everything after it', () => {
  const none = setupSteps({ ...fresh, podman: null, machine: null })
  expect(blocked(none)).toBe(true)
  expect(none.map((s) => s.id)).toEqual(['podman', 'service', 'cli'])
  expect(none[0].command).toBe('brew install podman')
  const noMachine = byId({ ...fresh, machine: 'missing' })
  expect(noMachine.machine.command).toBe('podman machine init')
  expect(blocked(setupSteps({ ...fresh, machine: 'missing' }))).toBe(true)
})

test('linux has no machine step', () => {
  expect(setupSteps({ ...fresh, platform: 'linux', machine: null }).map((s) => s.id)).toEqual(['podman', 'service', 'cli'])
})

test('everything done under the service', () => {
  const done = setupSteps({
    ...fresh,
    owner: 'service',
    node_version: '0.15.0',
    service_installed: true,
    service_running: true,
    cli_version: '0.15.0',
    machine: 'running',
  })
  expect(done.every((s) => s.done)).toBe(true)
})

test('a node someone else runs is not the service, and says so', () => {
  const s = byId({ ...fresh, owner: 'foreign', node_version: '0.14.0' })
  expect(s.service.done).toBe(false)
  expect(s.service.detail).toContain('Stop it')
})

test('an older CLI and a PATH that misses it are both left to do', () => {
  expect(byId({ ...fresh, cli_version: '0.14.0' }).cli.detail).toContain('v0.14.0')
  const hinted = byId({ ...fresh, cli_version: '0.15.0', path_hint: 'export PATH="/Users/op/.local/bin:$PATH"' })
  expect(hinted.cli.done).toBe(false)
  expect(hinted.cli.command).toContain('.local/bin')
})
