import { describe, expect, test } from 'bun:test'
import { tokens, wordCounts, wordDiff, type WordPart } from './word-diff'

const side = (parts: WordPart[], drop: WordPart['kind']) =>
  parts
    .filter((p) => p.kind !== drop)
    .map((p) => p.text)
    .join('')

describe('wordDiff', () => {
  test('marks the words that changed and keeps the rest', () => {
    expect(wordDiff('the old words here', 'the new words here')).toEqual([
      { kind: 'same', text: 'the ' },
      { kind: 'del', text: 'old' },
      { kind: 'add', text: 'new' },
      { kind: 'same', text: ' words here' },
    ])
  })

  test('each side reads back exactly', () => {
    const cases: [string, string][] = [
      ['', 'all new text'],
      ['all gone', ''],
      ['a b c d e', 'a c e f'],
      ['Line one.\n\nLine  two.', 'Line one.\nLine two, changed.'],
      ['same', 'same'],
    ]
    for (const [before, after] of cases) {
      const parts = wordDiff(before, after)
      expect(side(parts, 'add')).toBe(before)
      expect(side(parts, 'del')).toBe(after)
    }
    expect(wordDiff('same', 'same')).toEqual([{ kind: 'same', text: 'same' }])
  })

  test('a text too long to align is shown replaced, still exactly', () => {
    const before = Array.from({ length: 3000 }, (_, i) => `a${i}`).join(' ')
    const after = Array.from({ length: 3000 }, (_, i) => `b${i}`).join(' ')
    const parts = wordDiff(`x ${before} y`, `x ${after} y`)
    expect(side(parts, 'add')).toBe(`x ${before} y`)
    expect(side(parts, 'del')).toBe(`x ${after} y`)
    expect(parts[0]).toEqual({ kind: 'same', text: 'x ' })
  })

  test('counts words, not the spaces between them', () => {
    expect(tokens('a  b\nc')).toEqual(['a', '  ', 'b', '\n', 'c'])
    expect(wordCounts(wordDiff('one two three', 'one four five three'))).toEqual({ added: 2, removed: 1 })
  })
})
