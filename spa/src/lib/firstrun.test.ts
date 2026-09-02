import { expect, test } from 'bun:test'
import { nextStep, setupSteps } from './firstrun'

const fresh = { anyProviderConnected: false, anyChannel: false, hubPaired: false }

test('a node that can start a session shows nothing', () => {
  expect(setupSteps({ ...fresh, anyProviderConnected: true, anyChannel: true })).toBe(null)
  // The hub is offered, never required.
  expect(setupSteps({ anyProviderConnected: true, anyChannel: true, hubPaired: true })).toBe(null)
})

test('a fresh node is pointed at a provider first', () => {
  const steps = setupSteps(fresh)!
  expect(steps.map((s) => s.href)).toEqual(['/nodes', '/settings', '/settings#mesh'])
  expect(nextStep(steps).href).toBe('/nodes')
  expect(steps.every((s) => !s.done)).toBe(true)
})

test('the pointer moves to what is left', () => {
  const steps = setupSteps({ ...fresh, anyProviderConnected: true })!
  expect(nextStep(steps).title).toBe('Name a channel')
  // An optional step is never what the operator is sent to do.
  const hubOnly = setupSteps({ anyProviderConnected: true, anyChannel: false, hubPaired: true })!
  expect(nextStep(hubOnly).href).toBe('/settings')
})

test('the card returns when a provider goes away, however much has run', () => {
  const steps = setupSteps({ anyProviderConnected: false, anyChannel: true, hubPaired: true })
  expect(steps).not.toBe(null)
  expect(nextStep(steps!).href).toBe('/nodes')
  expect(steps!.find((s) => s.href === '/settings')!.done).toBe(true)
})
