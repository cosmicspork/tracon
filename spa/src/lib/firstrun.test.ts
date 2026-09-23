import { expect, test } from 'bun:test'
import { nextStep, setupSteps } from './firstrun'

const fresh = { anyProviderConnected: false, modelOffered: false, anyChannel: false, memberChannel: false, boundaryReady: false }

test('a refused boundary remains the first local blocker', () => {
  const steps = setupSteps({ ...fresh, anyProviderConnected: true, modelOffered: true, anyChannel: true, memberChannel: true })
  expect(nextStep(steps).href).toBe('/settings#maintenance')
})

test('preparation hands off to a local provider connection', () => {
  expect(nextStep(setupSteps({ ...fresh, boundaryReady: true })).href).toBe('/settings#connections')
})

test('a peer-only channel does not leave an empty checklist', () => {
  const steps = setupSteps({ ...fresh, boundaryReady: true, anyProviderConnected: true, anyChannel: true, modelOffered: true })
  expect(nextStep(steps).title).toBe('Make this node a channel member')
  expect(nextStep(steps).href).toBe('/settings#channels')
})

test('a disconnected provider blocks local work again', () => {
  const steps = setupSteps({ ...fresh, boundaryReady: true, anyChannel: true, memberChannel: true })
  expect(nextStep(steps).title).toBe('Connect a provider')
})

test('missing models point to declaration and channel scope before refresh', () => {
  const steps = setupSteps({ ...fresh, boundaryReady: true, anyProviderConnected: true, anyChannel: true, memberChannel: true })
  const detail = nextStep(steps).detail
  expect(detail).toContain('Declare models')
  expect(detail).toContain('channel scope')
  expect(detail).toContain('Refresh only if a declared model')
})
