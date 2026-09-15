import { expect, test } from 'bun:test'
import { parseTheme } from './theme'

test('only light and dark are explicit; anything else follows the system', () => {
  expect(parseTheme('light')).toBe('light')
  expect(parseTheme('dark')).toBe('dark')
  expect(parseTheme('auto')).toBe('auto')
  expect(parseTheme(null)).toBe('auto')
  expect(parseTheme('sepia')).toBe('auto')
})
