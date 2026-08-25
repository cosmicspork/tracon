import { expect, test } from 'bun:test'
import { classifyLines, splitDiff } from './diff'

const DIFF = `diff --git a/a.txt b/a.txt
index 111..222 100644
--- a/a.txt
+++ b/a.txt
@@ -1 +1,2 @@
 one
+two
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 333..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-was here`

test('a diff splits into its files with counts', () => {
  const files = splitDiff(DIFF)
  expect(files.map((f) => f.path)).toEqual(['a.txt', 'gone.txt'])
  expect(files[0].added).toBe(1)
  expect(files[0].removed).toBe(0)
  expect(files[1].removed).toBe(1)
})

test('a deleted file is named from the a-side, not /dev/null', () => {
  expect(splitDiff(DIFF)[1].path).toBe('gone.txt')
})

test('each file keeps its own hunks', () => {
  const files = splitDiff(DIFF)
  expect(files[0].lines.some((l) => l === '+two')).toBe(true)
  expect(files[0].lines.some((l) => l === '-was here')).toBe(false)
})

test('an empty diff yields no files rather than one empty one', () => {
  expect(splitDiff('')).toEqual([])
})

// A hunk whose content lines themselves start with - or +: the marker is the
// first character, so `--x` is a deletion and `++x` an addition, not headers.
const MARKER_DIFF = `diff --git a/f.sql b/f.sql
index 111..222 100644
--- a/f.sql
+++ b/f.sql
@@ -1,2 +1,2 @@
---old comment
+++new comment
 unchanged`

test('content lines starting with -- or ++ are classified by hunk position', () => {
  const lines = classifyLines(MARKER_DIFF.split('\n'))
  const kindOf = (text: string) => lines.find((l) => l.text === text)?.kind
  // Header region, before the first @@.
  expect(kindOf('--- a/f.sql')).toBe('meta')
  expect(kindOf('+++ b/f.sql')).toBe('meta')
  // Inside the hunk: the deleted line's content is `-old comment`.
  expect(kindOf('---old comment')).toBe('del')
  expect(kindOf('+++new comment')).toBe('add')
  expect(kindOf(' unchanged')).toBe('ctx')
})

test('splitDiff counts marker-prefixed content lines correctly', () => {
  const f = splitDiff(MARKER_DIFF)[0]
  // One real deletion and one real addition inside the hunk; the +++/--- file
  // headers are not counted.
  expect(f.added).toBe(1)
  expect(f.removed).toBe(1)
})
