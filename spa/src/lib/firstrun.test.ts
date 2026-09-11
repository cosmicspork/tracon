import { expect, test } from 'bun:test'
import { nextStep, setupSteps } from './firstrun'

const fresh = { anyProviderConnected: false, anyChannel: false, boundaryReady: false }

test('a ready local node needs no hub to start work', () => {
  expect(setupSteps({ anyProviderConnected: true, anyChannel: true, boundaryReady: true })).toBe(null)
})

test('a refused boundary blocks starting even with a connected provider', () => {
  const steps = setupSteps({ ...fresh, anyProviderConnected: true, anyChannel: true })!
  expect(nextStep(steps).href).toBe('/settings#boundary')
})

test('preparation hands off to the remaining connection requirement', () => {
  const steps = setupSteps({ ...fresh, boundaryReady: true })!
  expect(nextStep(steps).href).toBe('/nodes')
})

test('the card returns when a provider goes away, however much has run', () => {
  const steps = setupSteps({ anyProviderConnected: false, anyChannel: true, boundaryReady: true })
  expect(steps).not.toBe(null)
  expect(nextStep(steps!).href).toBe('/nodes')
  expect(steps!.find((s) => s.href === '/settings')!.done).toBe(true)
})
