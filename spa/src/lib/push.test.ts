import { expect, test } from 'bun:test'
import { keyBytes, needsInstall, supported } from './push'

test('the node key decodes to an uncompressed P-256 point', () => {
  const key = 'BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4'
  const bytes = keyBytes(key)
  expect(bytes.length).toBe(65)
  expect(bytes[0]).toBe(0x04)
})

test('padding is tolerated either way', () => {
  expect(keyBytes('AQ')).toEqual(new Uint8Array([1]))
  expect(keyBytes('AQ==')).toEqual(new Uint8Array([1]))
})

test('outside a browser nothing is supported and nothing needs installing', () => {
  expect(supported()).toBe(false)
  expect(needsInstall()).toBe(false)
})
