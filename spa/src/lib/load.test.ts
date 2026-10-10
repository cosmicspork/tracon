import { expect, test } from 'bun:test'
import { ApiError } from './api'
import { combineLoads, errorText, hasAnswer, isNotFound, LOADED, LOADING, loadPhase, settledLoad } from './load'

test('a list is loading, failed or answered', () => {
  expect(loadPhase(LOADING)).toBe('loading')
  expect(loadPhase(LOADED)).toBe('ready')
  expect(loadPhase({ loaded: false, error: 'database is locked' })).toBe('failed')
  expect(hasAnswer(LOADING)).toBe(false)
  expect(hasAnswer({ loaded: false, error: 'database is locked' })).toBe(false)
  expect(hasAnswer(LOADED)).toBe(true)
})

test('a refresh that fails after an answer keeps the answer and says it failed', () => {
  const failedRefresh = settledLoad(LOADED, { status: 'rejected', reason: new Error('database is locked') })
  expect(loadPhase(failedRefresh)).toBe('stale')
  expect(hasAnswer(failedRefresh)).toBe(true)
  expect(failedRefresh.error).toBe('database is locked')
  // The next refresh that answers clears it.
  expect(loadPhase(settledLoad(failedRefresh, { status: 'fulfilled', value: 1 }))).toBe('ready')
})

test('fetches needed together load once all have, and fail if any does', () => {
  expect(combineLoads(LOADED, LOADING)).toEqual(LOADING)
  expect(combineLoads(LOADED, LOADED)).toEqual(LOADED)
  expect(combineLoads(LOADING, { loaded: true, error: 'down' })).toEqual({ loaded: false, error: 'down' })
  expect(combineLoads()).toEqual(LOADED)
})

test('a settled snapshot answers, or fails keeping whether it ever answered', () => {
  const ok: PromiseSettledResult<number> = { status: 'fulfilled', value: 1 }
  const bad: PromiseSettledResult<number> = { status: 'rejected', reason: new Error('database is locked') }
  expect(settledLoad(LOADING, ok)).toEqual(LOADED)
  expect(settledLoad({ loaded: true, error: 'x' }, ok)).toEqual(LOADED)
  expect(settledLoad(LOADING, bad)).toEqual({ loaded: false, error: 'database is locked' })
  expect(settledLoad(LOADED, bad)).toEqual({ loaded: true, error: 'database is locked' })
})

test('an error reads as its message', () => {
  expect(errorText(new Error('boom'))).toBe('boom')
  expect(errorText('plain')).toBe('plain')
})

test('only a 404 means not found', () => {
  expect(isNotFound(new ApiError(404, 'no such approval'))).toBe(true)
  expect(isNotFound(new ApiError(500, 'database is locked'))).toBe(false)
  expect(isNotFound(new TypeError('Failed to fetch'))).toBe(false)
})
