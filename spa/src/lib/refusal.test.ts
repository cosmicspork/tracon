import { expect, test } from 'bun:test'
import { remedy } from './refusal'

test('every check the boundary can fail names what fixes it', () => {
  for (const check of [
    'runtime',
    'harness_unprivileged',
    'no_runtime_socket',
    'network_isolated',
    'egress',
  ]) {
    expect(remedy(check)).toMatch(/^(start|run) /)
  }
  // The runtime failure is the one an operator hits first, and podman not
  // being found is not the same as podman not running.
  expect(remedy('runtime')).toContain('node.toml')
})

test('an unknown or absent check still says where to look', () => {
  const fallback = 'run `tracon check-boundary --deep` at the node for detail'
  expect(remedy('something_new')).toBe(fallback)
  expect(remedy(null)).toBe(fallback)
  expect(remedy(undefined)).toBe(fallback)
})
