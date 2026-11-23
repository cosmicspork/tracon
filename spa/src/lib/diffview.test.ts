import { expect, test } from 'bun:test'
import { Chunk } from '@codemirror/merge'
import { Text } from '@codemirror/state'

import { splitDiff } from './diff'
import { MODE_KEY, defaultMode, fileSides, storedMode, storeMode, SPLIT_MIN_WIDTH } from './diffview'
import { overrideFor } from './mergeChanges'

const DIFF = `diff --git a/x.rs b/x.rs
index 111..222 100644
--- a/x.rs
+++ b/x.rs
@@ -1,4 +1,5 @@ fn top()
 one
-two
+TWO
+three
 four
@@ -10,3 +11,2 @@
 ten
--flag
 twelve
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+a
+b
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-was here
\\ No newline at end of file
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
`

const sides = splitDiff(DIFF).map(fileSides)

/** The chunks the merge view would mark, as 1-based row ranges on each side ("-" for none). */
function marked(i: number): string[] {
  const s = sides[i]
  const a = Text.of(s.a.split('\n'))
  const b = Text.of(s.b.split('\n'))
  const rows = (doc: Text, from: number, to: number) =>
    to > from ? `${doc.lineAt(from).number}-${doc.lineAt(to - 1).number}` : '-'
  return Chunk.build(a, b, { override: overrideFor(s) }).map((c) => `${rows(a, c.fromA, c.toA)}/${rows(b, c.fromB, c.toB)}`)
}

test('each side is its hunks, with every hunk header a row both share', () => {
  const [x] = sides
  expect(x.a.split('\n')).toEqual(['@@ -1,4 +1,5 @@ fn top()', 'one', 'two', 'four', '@@ -10,3 +11,2 @@', 'ten', '-flag', 'twelve', ''])
  expect(x.b.split('\n')).toEqual(['@@ -1,4 +1,5 @@ fn top()', 'one', 'TWO', 'three', 'four', '@@ -10,3 +11,2 @@', 'ten', 'twelve', ''])
})

test("each row carries its line in the real file, and a header row none", () => {
  const [x] = sides
  expect(x.aLines).toEqual([null, 1, 2, 3, null, 10, 11, 12, null])
  expect(x.bLines).toEqual([null, 1, 2, 3, 4, null, 11, 12, null])
  expect(sides[1].bLines).toEqual([null, 1, 2, null])
})

test("the merge view marks exactly the diff's changed rows, never a context row", () => {
  // A replacement of "two" by two rows, and a removed row whose text starts
  // with "-": both sides of each, and nothing around them.
  expect(marked(0)).toEqual(['3-3/3-4', '7-7/-'])
  // A new file: rows added after the header, none removed.
  expect(marked(1)).toEqual(['-/2-3'])
  // A deleted file, its "no newline" marker no row of its own.
  expect(marked(2)).toEqual(['2-2/-'])
})

test('what the header says is kept when no hunk shows it', () => {
  expect(sides[1].notes).toEqual(['new file'])
  expect(sides[2].notes).toEqual(['deleted'])
  expect(sides[3]).toMatchObject({ hunks: false, notes: ['binary file changed'], a: '', b: '' })
})

test('documents other than the diff’s are diffed afresh', () => {
  expect(overrideFor(sides[0])('x', 'y')).toHaveLength(1)
})

test('side by side is the default when the diff is wide enough, unified when not', () => {
  expect(defaultMode(SPLIT_MIN_WIDTH)).toBe('split')
  expect(defaultMode(1064)).toBe('split')
  expect(defaultMode(SPLIT_MIN_WIDTH - 1)).toBe('unified')
  expect(defaultMode(366)).toBe('unified')
})

test('the chosen mode is remembered, and storage that refuses only forgets it', () => {
  const kept = new Map<string, string>()
  const storage = { getItem: (k: string) => kept.get(k) ?? null, setItem: (k: string, v: string) => void kept.set(k, v) }
  expect(storedMode(storage)).toBeNull()
  storeMode('unified', storage)
  expect(kept.get(MODE_KEY)).toBe('unified')
  expect(storedMode(storage)).toBe('unified')
  kept.set(MODE_KEY, 'sideways')
  expect(storedMode(storage)).toBeNull()

  const refusing = {
    getItem: () => {
      throw new Error('SecurityError')
    },
    setItem: () => {
      throw new Error('QuotaExceededError')
    },
  }
  expect(storedMode(refusing)).toBeNull()
  expect(() => storeMode('split', refusing)).not.toThrow()
})
