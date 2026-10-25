import { describe, expect, test } from 'bun:test'
import { diffArguments, diffLines, hunks, type Hunk, type LineChange } from './text-diff'

const show = (lines: LineChange[]) =>
  lines.map((l) => `${l.kind === 'add' ? '+' : l.kind === 'del' ? '-' : ' '}${l.text}`)

describe('diffLines', () => {
  test('identical text is all context', () => {
    expect(show(diffLines('a\nb', 'a\nb'))).toEqual([' a', ' b'])
  })

  test('a replaced line reads as removal then addition, between its context', () => {
    expect(show(diffLines('one\ntwo\nthree', 'one\n2\nthree'))).toEqual([' one', '-two', '+2', ' three'])
  })

  test('insertions and deletions keep the common lines in order', () => {
    expect(show(diffLines('a\nb\nc\nd', 'a\nc\nx\nd\ne'))).toEqual([' a', '-b', ' c', '+x', ' d', '+e'])
  })

  test('against empty text the other side is all added, or all removed', () => {
    expect(show(diffLines('', 'new\ndoc'))).toEqual(['+new', '+doc'])
    expect(diffLines('', '# Plan\n\nBody.\n').every((l) => l.kind === 'add')).toBe(true)
    expect(show(diffLines('gone', ''))).toEqual(['-gone'])
    expect(diffLines('', '')).toEqual([])
  })

  test('a moved block costs the fewest changed lines', () => {
    const before = ['h1. Scope', 'x', 'y', 'h1. Risks', 'r'].join('\n')
    const after = ['h1. Risks', 'r', 'h1. Scope', 'x', 'y'].join('\n')
    const changed = diffLines(before, after).filter((l) => l.kind !== 'same')
    expect(changed).toHaveLength(4)
  })
})

describe('diffArguments', () => {
  test('only changed arguments are listed, in the original order then new ones', () => {
    const original = { project: 'WRK', summary: 'Old', description: 'a\nb', labels: ['x'] }
    const edited = { project: 'WRK', summary: 'New', description: 'a\nb', labels: ['x', 'y'], parent: 'WRK-1' }
    const changes = diffArguments(original, edited)
    expect(changes.map((c) => [c.key, c.kind])).toEqual([
      ['summary', 'changed'],
      ['labels', 'changed'],
      ['parent', 'added'],
    ])
    expect(show(changes[0]!.lines)).toEqual(['-Old', '+New'])
    expect(show(changes[1]!.lines)).toEqual([' [', '-  "x"', '+  "x",', '+  "y"', ' ]'])
    expect(show(changes[2]!.lines)).toEqual(['+WRK-1'])
  })

  test('a dropped argument is removed, and nothing changed is empty', () => {
    expect(diffArguments({ a: 'x', b: 'y' }, { a: 'x' })).toEqual([
      { key: 'b', kind: 'removed', lines: [{ kind: 'del', text: 'y' }] },
    ])
    expect(diffArguments({ a: 1, b: [1] }, { b: [1], a: 1 })).toEqual([])
  })
})

describe('hunks', () => {
  const lines = (n: number, from = 1) => Array.from({ length: n }, (_, i) => `line ${from + i}`)
  const fold = (h: Hunk[]) => h.map((x) => (x.kind === 'skip' ? `… ${x.count}` : show(x.lines).join('|')))

  test('a change in a long document keeps its context and counts the rest', () => {
    const before = lines(20).join('\n')
    const after = [...lines(9), 'changed', ...lines(10, 11)].join('\n')
    expect(fold(hunks(diffLines(before, after)))).toEqual([
      '… 6',
      ' line 7| line 8| line 9|-line 10|+changed| line 11| line 12| line 13',
      '… 7',
    ])
  })

  test('two changes close together share one run of lines', () => {
    const before = lines(10).join('\n')
    const after = ['x', ...lines(4, 2), 'y', ...lines(4, 7)].join('\n')
    const h = hunks(diffLines(before, after), 2)
    expect(h.filter((x) => x.kind === 'lines')).toHaveLength(1)
    expect(fold(h).at(-1)).toBe('… 2')
  })

  test('a run no longer than its context either side is not folded', () => {
    const before = ['a', ...lines(6), 'b'].join('\n')
    const after = ['A', ...lines(6), 'B'].join('\n')
    expect(hunks(diffLines(before, after)).every((x) => x.kind === 'lines')).toBe(true)
  })

  test('text with no changes folds to one skip, and a new document to one run of additions', () => {
    expect(hunks(diffLines('a\nb', 'a\nb'))).toEqual([{ kind: 'skip', count: 2 }])
    expect(fold(hunks(diffLines('', 'a\nb')))).toEqual(['+a|+b'])
  })
})
