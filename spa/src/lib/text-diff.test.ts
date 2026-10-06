import { describe, expect, test } from 'bun:test'
import { diffArguments, diffLines, type LineChange } from './text-diff'

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
    expect(show(diffLines('', 'new'))).toEqual(['-', '+new'])
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
