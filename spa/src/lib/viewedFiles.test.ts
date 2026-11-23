import { plugin } from 'bun'
import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

import { compileModule } from 'svelte/compiler'

import { splitDiff } from './diff'

// A rune module is compiled the way the build compiles it: TypeScript out
// first, then Svelte's runes.
plugin({
  name: 'svelte-module',
  setup(b) {
    b.onLoad({ filter: /\.svelte\.ts$/ }, ({ path }) => {
      const js = new Bun.Transpiler({ loader: 'ts' }).transformSync(readFileSync(path, 'utf8'))
      return { contents: compileModule(js, { filename: path, generate: 'client' }).js.code, loader: 'js' }
    })
  },
})

const { ViewedFiles, fileHash, loadMarks, stateOf, viewedKey } = await import('./viewedFiles.svelte')

const file = (path: string, at: number, body: string[], index = '1111111..2222222') =>
  `diff --git a/${path} b/${path}\nindex ${index} 100644\n--- a/${path}\n+++ b/${path}\n@@ -${at},3 +${at},3 @@ fn f()\n${body.join('\n')}\n`

const ONE = file('a.rs', 10, [' keep', '-old', '+new']) + file('b.rs', 1, [' x', '-y', '+z'])

function memory() {
  const kept = new Map<string, string>()
  return { kept, getItem: (k: string) => kept.get(k) ?? null, setItem: (k: string, v: string) => void kept.set(k, v) }
}

test("a file's fingerprint is its change, not where the base has moved it", () => {
  const [a] = splitDiff(file('a.rs', 10, [' keep', '-old', '+new']))
  // The same change twenty lines further down a base that moved, with other blobs.
  const [moved] = splitDiff(file('a.rs', 30, [' keep', '-old', '+new'], '3333333..4444444'))
  const [other] = splitDiff(file('a.rs', 10, [' keep', '-old', '+newer']))
  expect(fileHash(moved)).toBe(fileHash(a))
  expect(fileHash(other)).not.toBe(fileHash(a))
  // A binary file shows no lines: its blobs are all that say it changed.
  const bin = (index: string) =>
    splitDiff(`diff --git a/p.png b/p.png\nindex ${index} 100644\nBinary files a/p.png and b/p.png differ\n`)[0]
  expect(fileHash(bin('1..2'))).not.toBe(fileHash(bin('1..3')))
})

test('a mark is viewed while the file reads as it did, and changed once it does not', () => {
  expect(stateOf(undefined, 'h')).toBe('unviewed')
  expect(stateOf({ revision: 'r1', hash: 'h' }, 'h')).toBe('viewed')
  expect(stateOf({ revision: 'r1', hash: 'h' }, 'g')).toBe('changed')
})

test('marks are kept by review, on the revision they were made on', () => {
  const storage = memory()
  const v = new ViewedFiles('rv', 'r1', splitDiff(ONE), storage)
  expect(v.progress).toEqual({ viewed: 0, of: 2 })
  v.set('a.rs', true)
  expect(v.state('a.rs')).toBe('viewed')
  expect(v.progress).toEqual({ viewed: 1, of: 2 })
  expect(loadMarks('rv', storage)['a.rs'].revision).toBe('r1')
  expect(storage.kept.has(viewedKey('rv'))).toBe(true)
  // A file the revision does not have cannot be marked.
  v.set('elsewhere.rs', true)
  expect(v.tracks('elsewhere.rs')).toBe(false)
  expect(loadMarks('rv', storage)['elsewhere.rs']).toBeUndefined()
  v.set('a.rs', false)
  expect(v.state('a.rs')).toBe('unviewed')
})

test('a resubmission keeps the marks of files it left alone and clears the ones it changed', () => {
  const storage = memory()
  const first = new ViewedFiles('rv', 'r1', splitDiff(ONE), storage)
  first.set('a.rs', true)
  first.set('b.rs', true)
  // Revision 2 changed b.rs again and left a.rs as it was.
  const TWO = file('a.rs', 10, [' keep', '-old', '+new']) + file('b.rs', 1, [' x', '-y', '+zz'])
  const second = new ViewedFiles('rv', 'r2', splitDiff(TWO), storage)
  expect(second.state('a.rs')).toBe('viewed')
  expect(second.state('b.rs')).toBe('changed')
  expect(second.progress).toEqual({ viewed: 1, of: 2 })
  // Marked again, the mark is bound to the new revision.
  second.set('b.rs', true)
  expect(loadMarks('rv', storage)['b.rs'].revision).toBe('r2')
  // Another review's marks are its own.
  expect(new ViewedFiles('other', 'r1', splitDiff(ONE), storage).progress.viewed).toBe(0)
})

test('storage that refuses still lets the reader mark files for this page', () => {
  const refusing = {
    getItem: () => {
      throw new Error('SecurityError')
    },
    setItem: () => {
      throw new Error('SecurityError')
    },
  }
  const v = new ViewedFiles('rv', 'r1', splitDiff(ONE), refusing)
  v.set('a.rs', true)
  expect(v.state('a.rs')).toBe('viewed')
})
