import { expect, test } from 'bun:test'
import { nextStep, setupSteps } from './firstrun'

const fresh = { anyProviderConnected: false, modelOffered: false, anyChannel: false, boundaryReady: false }

test('a ready local node with an offered model needs no hub to start work', () => {
  expect(setupSteps({ anyProviderConnected: true, modelOffered: true, anyChannel: true, boundaryReady: true })).toBe(null)
})

test('a refused boundary blocks starting even with a connected provider', () => {
  const steps = setupSteps({ ...fresh, anyProviderConnected: true, modelOffered: true, anyChannel: true })!
  expect(nextStep(steps).href).toBe('/settings#maintenance')
})

test('preparation hands off to the local provider connection', () => {
  const steps = setupSteps({ ...fresh, boundaryReady: true })!
  expect(nextStep(steps).href).toBe('/settings#connections')
})

test('the local card returns when its provider goes away', () => {
  const steps = setupSteps({ anyProviderConnected: false, modelOffered: false, anyChannel: true, boundaryReady: true })
  expect(steps).not.toBe(null)
  expect(nextStep(steps!).href).toBe('/settings#connections')
  expect(steps!.find((s) => s.href === '/settings#channels')!.done).toBe(true)
})

test('a connected provider without an offered model remains blocked', () => {
  const steps = setupSteps({ anyProviderConnected: true, modelOffered: false, anyChannel: true, boundaryReady: true })!
  expect(nextStep(steps).title).toBe('Offer a model')
})

test('a connected provider with no model is told what to do about it', () => {
  const steps = setupSteps({ anyProviderConnected: true, modelOffered: false, anyChannel: true, boundaryReady: true })!
  expect(nextStep(steps).detail).toContain('Refresh models')
  expect(nextStep(steps).detail).toContain('channel scope')
})

test('before a provider is connected the step does not point at a refresh', () => {
  const steps = setupSteps({ ...fresh, boundaryReady: true })!
  const offer = steps.find((s) => s.title === 'Offer a model')!
  expect(offer.detail).not.toContain('Refresh models')
})

test('a step already done never reads as though it were still blocked', () => {
  const steps = setupSteps({ anyProviderConnected: true, modelOffered: true, anyChannel: true, boundaryReady: false })!
  const offer = steps.find((s) => s.title === 'Offer a model')!
  expect(offer.done).toBe(true)
  expect(offer.detail).not.toContain('Refresh models')
})
