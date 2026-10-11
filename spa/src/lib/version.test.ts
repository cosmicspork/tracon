import { expect, test } from 'bun:test'
import { staleInterface, versionLabel } from './version'

test('a page is stale only when both releases are known and differ', () => {
  expect(staleInterface({ version: '0.21.0' }, { version: '0.22.0' })).toBe(true)
  expect(staleInterface({ version: '0.22.0' }, { version: '0.22.0' })).toBe(false)
  expect(staleInterface({ version: '' }, { version: '0.22.0' })).toBe(false)
  expect(staleInterface({ version: '0.22.0' }, { version: undefined })).toBe(false)
  expect(staleInterface({ version: '0.22.0' }, { version: null })).toBe(false)
})

test('within one release, the builds decide when the node names its own', () => {
  // A dev build swapped in under a page loaded from the release.
  expect(staleInterface({ version: '0.30.0', build: '' }, { version: '0.30.0', build: 'edb04a39c1f2' })).toBe(true)
  // And back.
  expect(staleInterface({ version: '0.30.0', build: 'edb04a39c1f2' }, { version: '0.30.0', build: '' })).toBe(true)
  expect(staleInterface({ version: '0.30.0', build: 'edb04a39c1f2' }, { version: '0.30.0', build: 'edb04a39c1f2' })).toBe(false)
  expect(staleInterface({ version: '0.30.0', build: '' }, { version: '0.30.0', build: '' })).toBe(false)
  // An older node, or a bundle without an id, says nothing about builds.
  expect(staleInterface({ version: '0.30.0', build: 'edb04a39c1f2' }, { version: '0.30.0' })).toBe(false)
  expect(staleInterface({ version: '0.30.0', build: 'edb04a39c1f2' }, { version: '0.30.0', build: null })).toBe(false)
})

test('a release reads as its version, and any other build says so', () => {
  expect(versionLabel('0.30.0')).toBe('v0.30.0')
  expect(versionLabel('0.30.0', '')).toBe('v0.30.0')
  expect(versionLabel('0.30.0', null)).toBe('v0.30.0')
  expect(versionLabel('0.30.0', 'edb04a39c1f2')).toBe('v0.30.0 · dev edb04a39')
  expect(versionLabel(undefined, 'edb04a39c1f2')).toBe('')
})
