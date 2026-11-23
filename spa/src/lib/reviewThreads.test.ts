import { expect, test } from 'bun:test'
import { classifyLines } from './diff'
import {
  anchorAt,
  anchorProblem,
  createBody,
  numberLines,
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
