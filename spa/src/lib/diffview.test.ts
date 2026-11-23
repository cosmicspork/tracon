import { expect, test } from 'bun:test'
import { Chunk } from '@codemirror/merge'
import { Text } from '@codemirror/state'

import { splitDiff } from './diff'
import {
  MODE_KEY,
  WRAP_KEY,
  LARGE_LINES,
  LONG_LINE,
  contextMismatch,
  defaultMode,
  estimatedRows,
  expand,
  heavy,
  fileSides,
  noLines,
  parseHunks,
  storedMode,
  storedWrap,
  storeMode,
  storeWrap,
  SPLIT_MIN_WIDTH,
} from './diffview'
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
  expect(sides[3]).toMatchObject({ hunks: false, notes: ['binary'], a: '', b: '' })
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

// --- context around the hunks ---------------------------------------------

const base40 = Array.from({ length: 40 }, (_, i) => `line ${i + 1}`)
const head40 = base40.map((l, i) => (i === 4 ? 'line five' : l))
// Line 5 changed, three lines of context either side: the diff leaves out
// line 1 above and lines 9 to 40 below.
const ONE = `diff --git a/f b/f
index 1111111..2222222 100644
--- a/f
+++ b/f
@@ -2,7 +2,7 @@ fn top()
 line 2
 line 3
 line 4
-line 5
+line five
 line 6
 line 7
 line 8
`
const one = splitDiff(ONE)[0]
const pinned = { base: base40.join('\n') + '\n', head: head40.join('\n') + '\n' }

test('the gaps a diff leaves out are counted from its hunk headers, the last once the file is known', () => {
  expect(expand(one, null, new Set()).gaps).toEqual([{ gap: 0, hidden: 1 }])
  expect(expand(one, null, new Set()).tail).toEqual({ gap: 1, hidden: null })
  expect(expand(one, pinned, new Set()).tail).toEqual({ gap: 1, hidden: 32 })
  // Without the files, opening a gap shows nothing new.
  expect(expand(one, null, new Set([0, 1])).file.lines).toEqual(one.lines)
})

test('an opened gap is written into the diff as context, numbered on both sides', () => {
  const all = expand(one, pinned, new Set([0, 1]))
  expect(all.gaps).toEqual([{ gap: 0, hidden: 0 }])
  expect(all.tail).toBeNull()
  const s = fileSides(all.file)
  expect(s.a.split('\n')[0]).toBe('@@ -1,40 +1,40 @@ fn top()')
  expect(s.aLines.filter((n) => n !== null)).toEqual(Array.from({ length: 40 }, (_, i) => i + 1))
  expect(s.bLines.filter((n) => n !== null)).toEqual(Array.from({ length: 40 }, (_, i) => i + 1))
  expect(s.b.split('\n')[5]).toBe('line five')
  // The marked change is still only the changed line.
  expect(s.blocks).toEqual([{ a: 5, aN: 1, b: 5, bN: 1 }])
})

test('the "before" column of a unified view numbers context rows only', () => {
  const s = fileSides(one)
  expect(s.bOld).toEqual([null, 2, 3, 4, null, 6, 7, 8, null])
  expect(s.bLines).toEqual([null, 2, 3, 4, 5, 6, 7, 8, null])
})

test('files that do not match the diff are refused as context', () => {
  const { hunks } = parseHunks(one)
  expect(contextMismatch(hunks, base40, head40)).toBeNull()
  expect(contextMismatch(hunks, null, head40)).toBeNull()
  expect(contextMismatch(hunks, base40, base40)).toBe('the changed file differs from the diff at line 5')
  expect(contextMismatch(hunks, head40, null)).toBe('the base file differs from the diff at line 5')
})

test('a new file has nothing after its one hunk to show', () => {
  const added = splitDiff(DIFF)[1]
  expect(expand(added, null, new Set()).tail).toBeNull()
})

test('a binary file, a rename and a mode change each say what happened', () => {
  expect(noLines(sides[3])).toBe('Binary file: its contents are not shown as text.')
  expect(noLines({ notes: ['renamed from a.rs'] })).toBe('Renamed from a.rs; the contents did not change.')
  expect(noLines({ notes: ['mode now 100755'] })).toBe('Only the file mode changed.')
})

test('wrapping is off until the viewer turns it on, and survives storage that refuses', () => {
  const kept = new Map<string, string>()
  const storage = { getItem: (k: string) => kept.get(k) ?? null, setItem: (k: string, v: string) => void kept.set(k, v) }
  expect(storedWrap(storage)).toBe(false)
  storeWrap(true, storage)
  expect(kept.get(WRAP_KEY)).toBe('on')
  expect(storedWrap(storage)).toBe(true)
  const refusing = {
    getItem: () => {
      throw new Error('SecurityError')
    },
    setItem: () => {
      throw new Error('SecurityError')
    },
  }
  expect(storedWrap(refusing)).toBe(false)
  storeWrap(true, refusing)
})

// --- files too heavy to open by default -----------------------------------

const made = (path: string, lines: string[], added = 1, removed = 0) => ({ path, added, removed, lines })

test('lockfiles, bundles and generated code are known by their paths', () => {
  expect(heavy(made('Cargo.lock', []))).toEqual({ kind: 'generated', why: 'Generated file (lockfile)' })
  expect(heavy(made('web/package-lock.json', []))?.why).toBe('Generated file (lockfile)')
  expect(heavy(made('static/app.min.js', []))?.why).toBe('Generated file (minified or a source map)')
  expect(heavy(made('api/v1/service.pb.go', []))?.why).toBe('Generated file (generated code)')
  expect(heavy(made('src/lib.rs', [' fn main() {}', '+let x = 1;']))).toBeNull()
  // A lock in the name is not a lockfile.
  expect(heavy(made('src/lock.rs', []))).toBeNull()
})

test('a file that says it is generated in its first lines is generated', () => {
  const lines = ['@@ -0,0 +1,3 @@', '+// Code generated by protoc-gen-go. DO NOT EDIT.', '+package api']
  expect(heavy(made('api/types.go', lines))?.kind).toBe('generated')
  // Saying so further down is just a line of code.
  const later = ['@@ -0,0 +1,12 @@', ...Array.from({ length: 10 }, (_, i) => `+line ${i}`), '+// @generated']
  expect(heavy(made('src/x.ts', later))).toBeNull()
})

test('a diff of thousands of lines, or of very long lines, is large', () => {
  expect(heavy(made('src/big.rs', [], LARGE_LINES, 1))).toEqual({ kind: 'large', why: 'Large diff: 1,501 changed lines' })
  expect(heavy(made('src/big.rs', [], LARGE_LINES, 0))).toBeNull()
  expect(heavy(made('data/x.json', ['@@ -1 +1 @@', `+${'x'.repeat(LONG_LINE + 1)}`]))?.why).toBe(
    'Very long lines: one is 5,002 characters',
  )
})

test("a file's place is held at the rows its merge view will take", () => {
  const [x] = sides
  // Side by side, a removed row with no added row opposite takes a row of its own.
  expect(estimatedRows(x, 'split')).toBe(x.bLines.length + 1)
  // Unified, every removed row does.
  expect(estimatedRows(x, 'unified')).toBe(x.bLines.length + 2)
})
