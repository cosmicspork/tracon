import { expect, test } from 'bun:test'
import { classifyLines, splitDiff } from './diff'
import { expand, fileSides } from './diffview'
import {
  anchorAt,
  threadsBlockedReason,
  anchorProblem,
  createBody,
  inDiff,
  numberLines,
  sideRows,
  threadsOn,
  unresolvedCount,
  whereLabel,
  type ReviewThread,
} from './reviewThreads'

const DIFF = `diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@
 fn one() {}
-fn two() {}
+fn two() -> u8 { 2 }
+fn three() {}
 fn four() {}
 fn five() {}
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-was here`

const lines = numberLines(classifyLines(DIFF.split('\n')))
const at = (text: string) => lines.find((l) => l.text === text)!

function thread(over: Partial<ReviewThread>): ReviewThread {
  return {
    id: 't',
    kind: 'line',
    revision_id: 'rev1',
    revision: 1,
    state: 'unresolved',
    outdated: false,
    path: 'src/a.rs',
    side: 'new',
    start_line: 2,
    end_line: 3,
    comments: [],
    created_ms: 0,
    updated_ms: 0,
    ...over,
  }
}

test('each diff line is numbered on the side it is on, as the node numbers it', () => {
  expect(at(' fn one() {}')).toMatchObject({ path: 'src/a.rs', old: 1, new: 1 })
  expect(at('-fn two() {}')).toMatchObject({ old: 2, new: null })
  expect(at('+fn two() -> u8 { 2 }')).toMatchObject({ old: null, new: 2 })
  expect(at('+fn three() {}')).toMatchObject({ new: 3 })
  expect(at(' fn five() {}')).toMatchObject({ old: 4, new: 5 })
  // A header has none, and a deletion is on the old side of its own path.
  expect(at('+++ b/src/a.rs')).toMatchObject({ old: null, new: null })
  expect(at('-was here')).toMatchObject({ path: 'gone.txt', old: 1, new: null })
})

test('a picked line starts a thread there, and a second pick makes a range', () => {
  const first = anchorAt(at('+fn three() {}'))
  expect(first).toEqual({ kind: 'line', path: 'src/a.rs', side: 'new', start_line: 3, end_line: 3 })
  expect(anchorAt(at('+fn two() -> u8 { 2 }'), first)).toEqual({
    kind: 'line',
    path: 'src/a.rs',
    side: 'new',
    start_line: 2,
    end_line: 3,
  })
  // A removed line is on the base; picking it starts afresh rather than mixing sides.
  expect(anchorAt(at('-fn two() {}'), first)).toEqual({ kind: 'line', path: 'src/a.rs', side: 'old', start_line: 2, end_line: 2 })
  expect(anchorAt(at('@@ -1,4 +1,5 @@'))).toBeNull()
})

test('a line carries only the threads written on the revision shown', () => {
  const here = thread({ id: 'here' })
  const earlier = thread({ id: 'earlier', revision_id: 'rev0' })
  const base = thread({ id: 'base', side: 'old', start_line: 2, end_line: 2 })
  const ids = (l: string) => threadsOn([here, earlier, base], at(l), 'rev1').map((t) => t.id)
  expect(ids('+fn three() {}')).toEqual(['here'])
  expect(ids('-fn two() {}')).toEqual(['base'])
  expect(ids(' fn five() {}')).toEqual([])
})

test('where a thread is reads in words', () => {
  expect(whereLabel(thread({}))).toBe('src/a.rs:2–3')
  expect(whereLabel(thread({ end_line: 2 }))).toBe('src/a.rs:2')
  expect(whereLabel(thread({ side: 'old', end_line: 2 }))).toBe('src/a.rs:2 · base')
  expect(whereLabel(thread({ kind: 'file' }))).toBe('src/a.rs')
  expect(whereLabel(thread({ kind: 'general', path: null }))).toBe('General')
})

test('what the agent has still to answer counts outdated threads, not addressed or resolved ones', () => {
  expect(
    unresolvedCount([
      thread({ state: 'unresolved' }),
      thread({ state: 'outdated' }),
      thread({ state: 'addressed' }),
      thread({ state: 'resolved' }),
    ]),
  ).toBe(2)
})

test('a thread is sent with only what its kind names', () => {
  expect(anchorProblem({ kind: 'general' }, '  ')).toBe('say something')
  expect(anchorProblem({ kind: 'file' }, 'x')).toBe('choose a file')
  expect(anchorProblem({ kind: 'line', path: 'a', start_line: 3, end_line: 2 }, 'x')).toBe('the range ends before it starts')
  expect(anchorProblem({ kind: 'line', path: 'a', start_line: 3 }, 'x')).toBeNull()
  expect(createBody({ kind: 'general', path: 'a', start_line: 3 }, ' why? ', 'rev1')).toEqual({
    kind: 'general',
    body: 'why?',
    revision_id: 'rev1',
    path: undefined,
    side: undefined,
    start_line: undefined,
    end_line: undefined,
  })
  expect(createBody({ kind: 'line', path: 'a', start_line: 3 }, 'x', null)).toMatchObject({
    kind: 'line',
    path: 'a',
    side: 'new',
    start_line: 3,
    end_line: 3,
    revision_id: undefined,
  })
})

// --- the merge view's rows -------------------------------------------------

const MORE = `diff --git a/src/b.ts b/src/b.ts
index 3..4 100644
--- a/src/b.ts
+++ b/src/b.ts
@@ -10,3 +10,3 @@ export function b() {
 const x = 1
-const y = 2
+const y = 3
 const z = 4
@@ -40,2 +40,3 @@
 last()
+added()
 end()
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
diff --git a/tail.txt b/tail.txt
--- a/tail.txt
+++ b/tail.txt
@@ -1,2 +1,2 @@
 keep
-old end
\\ No newline at end of file
+new end
\\ No newline at end of file`

const key = (l: { kind: string; text: string; path: string | null; old: number | null; new: number | null }) =>
  JSON.stringify([l.path, l.kind, l.text, l.old, l.new])

test('every merge-view row is the numbered diff line it shows, as numberLines numbers it', () => {
  for (const diff of [DIFF, MORE]) {
    for (const file of splitDiff(diff)) {
      const rows = sideRows(fileSides(file))
      const fromRows = new Set([...rows.a, ...rows.b].flatMap((l) => (l ? [key(l)] : [])))
      const fromDiff = numberLines(classifyLines(file.lines)).filter((l) => l.old !== null || l.new !== null)
      expect([...fromRows].sort()).toEqual(fromDiff.map(key).sort())
    }
  }
})

test('the merge view numbers each side: a removal on the old, an addition on the new, context on both', () => {
  const [b] = splitDiff(MORE)
  const sides = fileSides(b)
  const rows = sideRows(sides)
  const rowA = (text: string) => rows.a[sides.a.split('\n').indexOf(text)]
  const rowB = (text: string) => rows.b[sides.b.split('\n').indexOf(text)]
  // Hunk headers are rows of both sides but no line.
  expect(rows.a[0]).toBeNull()
  expect(rows.b[0]).toBeNull()
  expect(rowA('const y = 2')).toMatchObject({ path: 'src/b.ts', kind: 'del', old: 11, new: null })
  expect(rowB('const y = 3')).toMatchObject({ path: 'src/b.ts', kind: 'add', old: null, new: 11 })
  // A context row is the same line whichever side it is picked on.
  expect(rowA('const z = 4')).toEqual(rowB('const z = 4'))
  expect(rowA('const z = 4')).toMatchObject({ kind: 'ctx', old: 12, new: 12 })
  // After an addition the two sides number context apart.
  expect(rowB('end()')).toMatchObject({ kind: 'ctx', old: 41, new: 42 })
  // The empty row both sides end with is no line.
  expect(rows.a.at(-1)).toBeNull()
  expect(rows.b.at(-1)).toBeNull()
})

test('a line picked in the merge view anchors and marks as one picked in the plain diff', () => {
  const [a] = splitDiff(DIFF)
  const sides = fileSides(a)
  const rows = sideRows(sides)
  const added = rows.b[sides.b.split('\n').indexOf('fn three() {}')]!
  expect(anchorAt(added)).toEqual(anchorAt(at('+fn three() {}')))
  const removed = rows.a[sides.a.split('\n').indexOf('fn two() {}')]!
  expect(anchorAt(removed)).toEqual(anchorAt(at('-fn two() {}')))
  const t = [thread({ start_line: 3, end_line: 3 })]
  expect(threadsOn(t, added, 'rev1')).toHaveLength(1)
  expect(threadsOn(t, removed, 'rev1')).toHaveLength(0)
  // The context line after it is 5 on the new side: not on lines 2–3.
  const ctx = rows.a[sides.a.split('\n').indexOf('fn five() {}')]!
  expect(threadsOn([thread({ start_line: 5, end_line: 5 })], ctx, 'rev1')).toHaveLength(1)
  expect(threadsOn([thread({ side: 'old', start_line: 4, end_line: 4 })], ctx, 'rev1')).toHaveLength(1)
})

test('an opened gap numbers its lines as the file does, but no thread starts on them', () => {
  const [b] = splitDiff(MORE)
  const base = Array.from({ length: 50 }, (_, i) => `line ${i + 1}`)
  base.splice(9, 3, 'const x = 1', 'const y = 2', 'const z = 4')
  base.splice(39, 2, 'last()', 'end()')
  const head = [...base]
  head.splice(10, 1, 'const y = 3')
  head.splice(40, 0, 'added()')
  const open = expand(b, { base: base.join('\n'), head: head.join('\n') }, new Set([1]))
  const sides = fileSides(open.file)
  const rows = sideRows(sides)
  const rowB = (text: string) => rows.b[sides.b.split('\n').indexOf(text)]
  const rowA = (text: string) => rows.a[sides.a.split('\n').indexOf(text)]
  // The gap's lines read as the file numbers them, and the hunks either side keep their numbers.
  expect(rowB('line 20')).toMatchObject({ kind: 'ctx', old: 20, new: 20 })
  expect(rowB('const z = 4')).toMatchObject({ kind: 'ctx', old: 12, new: 12 })
  expect(rowB('added()')).toMatchObject({ kind: 'add', new: 41 })
  expect(rowB('end()')).toMatchObject({ kind: 'ctx', old: 41, new: 42 })
  expect(rowA('const y = 2')).toMatchObject({ kind: 'del', old: 11 })
  // Only the diff's own lines take a thread: the node anchors to nothing else.
  const pickable = inDiff(fileSides(b))
  expect(pickable(rowB('line 20')!)).toBe(false)
  expect(pickable(rowB('line 13')!)).toBe(false)
  for (const l of [rowB('const z = 4'), rowB('added()'), rowB('end()'), rowA('const y = 2'), rowB('const y = 3')]) {
    expect(pickable(l!)).toBe(true)
  }
})

test('a decided review says its threads are read-only; a held one names its node', () => {
  expect(threadsBlockedReason({ closed: true, remoteOwner: null, ownerFetched: false })).toBe('Decided: threads are read-only.')
  expect(threadsBlockedReason({ closed: true, remoteOwner: 'work-pod', ownerFetched: false })).toBe(
    'Decided: threads are read-only.',
  )
  expect(threadsBlockedReason({ closed: false, remoteOwner: 'work-pod', ownerFetched: false })).toBe(
    'Its threads are held by work-pod, which could not be read; reload once it can.',
  )
  expect(threadsBlockedReason({ closed: false, remoteOwner: 'work-pod', ownerFetched: true })).toBeNull()
  expect(threadsBlockedReason({ closed: false, remoteOwner: null, ownerFetched: false })).toBeNull()
})
