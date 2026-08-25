import { expect, test } from 'bun:test'
import { splitDiff } from './diff'

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
