import { expect, test } from 'bun:test'
import { permissionOptions, publicationFailure, type Permission, type Review } from './types'

const base: Permission = {
  id: 'p',
  session_id: 's',
  node_id: 'n',
  title: 't',
  kind: 'execute',
  raw_input: null,
  options: '[]',
  state: 'new',
  created_ms: 0,
  expires_ms: 0,
}

test('options parse from the wire camelCase the harness uses', () => {
  const p = {
    ...base,
    options: '[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"}]',
  }
  expect(permissionOptions(p)).toEqual([
    { option_id: 'allow_once', name: 'Allow once', kind: 'allow_once' },
  ])
})

test('snake_case and malformed options are tolerated', () => {
  expect(permissionOptions({ ...base, options: '[{"option_id":"x","name":"X","kind":"k"}]' })[0].option_id).toBe('x')
  expect(permissionOptions({ ...base, options: 'not json' })).toEqual([])
})

test('a failed publication is named only while the review is back with the operator', () => {
  const review = { state: 'claimed', publication_failed: 'feat/a is not a branch on the forge any more' } as Review
  expect(publicationFailure(review)).toBe('feat/a is not a branch on the forge any more')
  expect(publicationFailure({ ...review, state: 'new' })).toBe('feat/a is not a branch on the forge any more')
  expect(publicationFailure({ ...review, state: 'publishing' })).toBeNull()
  expect(publicationFailure({ ...review, publication_failed: undefined })).toBeNull()
})
