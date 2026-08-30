import { expect, test } from 'bun:test'

import { firstRunSteps, nextStep } from './firstrun'

test('a node that has run a session gets no checklist', () => {
  expect(
    firstRunSteps({ anyProviderConnected: true, anyReadyWork: true, anySession: true }),
  ).toBeNull()
})

test('a fresh node is pointed at the provider first', () => {
  const steps = firstRunSteps({
    anyProviderConnected: false,
    anyReadyWork: false,
    anySession: false,
  })!
  expect(steps).toHaveLength(3)
  expect(steps.map((s) => s.done)).toEqual([false, false, false])
  expect(nextStep(steps).href).toBe('/nodes')
})

test('progress marks steps done and moves the pointer', () => {
  const steps = firstRunSteps({
    anyProviderConnected: true,
    anyReadyWork: false,
    anySession: false,
  })!
  expect(steps[0].done).toBe(true)
  expect(nextStep(steps).href).toBe('/work')

  const ready = firstRunSteps({
    anyProviderConnected: true,
    anyReadyWork: true,
    anySession: false,
  })!
  expect(nextStep(ready).href).toBe('/new')
})

test('the last step is never marked done: doing it ends the checklist', () => {
  const steps = firstRunSteps({
    anyProviderConnected: true,
    anyReadyWork: true,
    anySession: false,
  })!
  expect(steps[2].done).toBe(false)
})
