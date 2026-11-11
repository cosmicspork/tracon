import { expect, test } from 'bun:test'
import { blocked, canRestartNode, nodeOwnerSummary, setupSteps, type SetupStatus } from './desktop-setup'

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
  service_failing: false,
  service_error: null,
  podman: null,
  container: '/usr/local/bin/container',
  services: 'running',
  services_error: null,
}

const byId = (s: SetupStatus) => Object.fromEntries(setupSteps(s).map((x) => [x.id, x]))

test('a fresh mac with container running is asked for the service and the CLI', () => {
  const steps = byId(fresh)
  expect(steps.container.done).toBe(true)
  expect(steps.services.done).toBe(true)
  expect(steps.podman).toBeUndefined()
  expect(steps.service.done).toBe(false)
  expect(steps.cli.done).toBe(false)
  expect(blocked(setupSteps(fresh))).toBe(false)
})

test('a mac without container is pointed at its package, never at Podman', () => {
  const none = setupSteps({ ...fresh, container: null, services: null })
  expect(blocked(none)).toBe(true)
  expect(none.map((s) => s.id)).toEqual(['container', 'service', 'cli'])
  expect(none[0].href).toBe('https://github.com/apple/container/releases')
  expect(none[0].hrefLabel).toBeDefined()
})

test('stopped and unaskable services both block setup with distinct recovery', () => {
  const stopped = byId({ ...fresh, services: 'stopped', services_error: 'apiserver is not running' })
  expect(stopped.services.command).toBe('container system start --enable-kernel-install')
  expect(stopped.services.reason).toBe('apiserver is not running')
  const failed = byId({ ...fresh, services: 'error', services_error: 'permission denied' })
  expect(failed.services.detail).toContain('too old or broken')
  expect(failed.services.command).toBe('container system status')
  expect(blocked(setupSteps({ ...fresh, services: 'stopped' }))).toBe(true)
  expect(blocked(setupSteps({ ...fresh, services: 'error' }))).toBe(true)
})

test('linux asks for Podman and has no container steps', () => {
  const linux = { ...fresh, platform: 'linux' as const, container: null, services: null }
  const missing = setupSteps({ ...linux, podman: null })
  expect(missing.map((s) => s.id)).toEqual(['podman', 'service', 'cli'])
  expect(missing[0].href).toBe('https://podman.io/docs/installation')
  expect(blocked(missing)).toBe(true)
  expect(blocked(setupSteps({ ...linux, podman: '/usr/bin/podman' }))).toBe(false)
})

test('everything done under the service', () => {
  const done = setupSteps({
    ...fresh,
    owner: 'service',
    node_version: '0.15.0',
    service_installed: true,
    service_running: true,
    cli_version: '0.15.0',
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

test('a service whose node keeps exiting says why, and can be restarted', () => {
  const failing: SetupStatus = {
    ...fresh,
    platform: 'linux',
    podman: '/usr/bin/podman',
    container: null,
    services: null,
    service_installed: true,
    service_failing: true,
    service_error: 'the `omp` harness was removed at the OpenCode cutover.',
  }
  const s = byId(failing)
  expect(s.service.done).toBe(false)
  expect(s.service.detail).toContain('keeps exiting')
  expect(s.service.reason).toContain('`omp` harness was removed')
  expect(nodeOwnerSummary(failing)).toBe('keeps exiting under the service')
  expect(canRestartNode(failing)).toBe(true)

  // Failing with nothing in the log: point at the supervisor instead.
  const silent = byId({ ...failing, service_error: null })
  expect(silent.service.reason).toBeUndefined()
  expect(silent.service.command).toBe('tracon service status')

  // Merely not answering yet is not failing.
  const starting = byId({ ...failing, service_failing: false, service_error: null })
  expect(starting.service.detail).toBe('Installed, but the node is not answering.')
  expect(nodeOwnerSummary({ ...failing, service_failing: false })).toBe('not answering under the service')
})

test('restart is offered whenever the unit is installed, and only then', () => {
  expect(canRestartNode(fresh)).toBe(false)
  expect(nodeOwnerSummary(fresh)).toBe('not running')
  const running: SetupStatus = { ...fresh, owner: 'service', service_installed: true, service_running: true }
  expect(canRestartNode(running)).toBe(true)
  expect(nodeOwnerSummary(running)).toBe('runs under the service')
  // Not a failure reason when nothing is failing, even if one was sent.
  expect(byId({ ...running, service_error: 'stale' }).service.reason).toBeUndefined()
})
