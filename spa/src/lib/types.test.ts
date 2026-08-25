import { expect, test } from 'bun:test'
import { permissionOptions, type Permission } from './types'

const base: Permission = {
  id: 'p',
  session_id: 's',
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
