import { expect, test } from 'bun:test'

import { columns, longSubject, realLines } from './linegutter'

test('a message splits into its real lines at each line feed', () => {
  expect(realLines('fix: subject\n\nbody line\nSigned-off-by: a')).toEqual(['fix: subject', '', 'body line', 'Signed-off-by: a'])
})

test('a carriage return and line feed is one break, not two', () => {
  expect(realLines('one\r\ntwo\r\n\r\nfour')).toEqual(['one', 'two', '', 'four'])
  expect(realLines('one\rtwo')).toEqual(['one', 'two'])
})

test('a trailing break leaves an empty last line to number', () => {
  expect(realLines('subject\n')).toEqual(['subject', ''])
  expect(realLines('subject\r\n')).toEqual(['subject', ''])
  expect(realLines('')).toEqual([''])
})

test('a long line stays one real line however wide it is', () => {
  expect(realLines('x'.repeat(300))).toHaveLength(1)
})

test('an astral character counts as one column', () => {
  expect(columns('ship 🚀')).toBe(6)
})

test('a subject past 72 columns is reported with its length', () => {
  expect(longSubject(`${'a'.repeat(73)}\n\nbody`)).toBe(73)
})

test('a subject of 72 columns or fewer is not reported, whatever the body', () => {
  expect(longSubject(`${'a'.repeat(72)}\n\n${'b'.repeat(200)}`)).toBeNull()
  expect(longSubject('')).toBeNull()
})
