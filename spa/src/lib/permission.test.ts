import { expect, test } from 'bun:test'
import { editableFields, editedArguments, isEgress } from './permission'

const card = (args: unknown, kind: string | null = 'tool') => ({
  kind,
  raw_input: JSON.stringify({ tool: 'issue_comment', arguments: args }),
})

test('the prose arguments of a brokered call are editable, identifiers are not', () => {
  const body = 'A comment long enough that it is prose, not an identifier.'
  expect(editableFields(card({ key: 'WRK-1', body }))).toEqual([{ key: 'body', value: body }])
  expect(editableFields(card({ key: 'WRK-1', body: 'one\ntwo' })).map((f) => f.key)).toEqual(['body'])
})

test("a harness's own request is never editable", () => {
  expect(editableFields(card({ body: 'x'.repeat(80) }, 'execute'))).toEqual([])
  expect(editableFields({ kind: 'tool', raw_input: 'not json' })).toEqual([])
})

test('only a change is sent, merged over the original arguments', () => {
  const c = card({ key: 'WRK-1', body: 'x'.repeat(50) })
  expect(editedArguments(c, {})).toBeUndefined()
  expect(editedArguments(c, { body: 'x'.repeat(50) })).toBeUndefined()
  expect(editedArguments(c, { body: 'rewritten' })).toEqual({ key: 'WRK-1', body: 'rewritten' })
})

test('an egress ask is told apart from a held call', () => {
  const ask = { raw_input: JSON.stringify({ tool: 'request_egress', arguments: { host: 'pypi.org' }, approval_id: 'a1' }) }
  expect(isEgress(ask)).toBe(true)
  expect(isEgress(card({ body: 'x' }))).toBe(false)
  expect(isEgress({ raw_input: null })).toBe(false)
})
