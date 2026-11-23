// What CodeMirror's merge view marks changed in a review's diff: the diff's
// own changes, not a fresh diff of the two sides. The diff is the unit of
// review, and what is marked changed should be exactly what git says changed.
// Kept apart from `diffview.ts` because it needs CodeMirror, which the page
// fetches only when a diff shows.

import { Change, presentableDiff } from '@codemirror/merge'

import type { FileSides } from './diffview'

/** Character offsets of the start of each row, plus one past the end. */
function offsets(rows: string[]): number[] {
  const at = [0]
  for (const r of rows) at.push(at[at.length - 1] + r.length + 1)
  return at
}

/** The changes of one file, in positions of its two sides. */
export function mergeChanges(sides: FileSides): Change[] {
  const a = sides.a.split('\n')
  const b = sides.b.split('\n')
  const offA = offsets(a)
  const offB = offsets(b)
  const changes: Change[] = []
  for (const k of sides.blocks) {
    if (k.aN > 0 && k.bN > 0) {
      // A replacement: which words changed inside it, so they read apart from
      // the lines around them.
      const fromA = offA[k.a]
      const fromB = offB[k.b]
      const textA = a.slice(k.a, k.a + k.aN).join('\n')
      const textB = b.slice(k.b, k.b + k.bN).join('\n')
      const inner = presentableDiff(textA, textB)
      if (inner.length === 0) {
        // The same text on both sides, which a newline at the end of the file
        // alone tells apart: the whole lines are the change.
        changes.push(new Change(fromA, fromA + textA.length, fromB, fromB + textB.length))
      } else {
        for (const c of inner) changes.push(new Change(fromA + c.fromA, fromA + c.toA, fromB + c.fromB, fromB + c.toB))
      }
    } else {
      // Whole lines added or removed. The range takes the line break after
      // them, so the line that follows is not marked; the shared empty row
      // both sides end with means there always is one.
      changes.push(new Change(offA[k.a], offA[k.a + k.aN], offB[k.b], offB[k.b + k.bN]))
    }
  }
  return changes
}

/**
 * The merge view's diff override for one file: the diff's own changes, as
 * long as the documents are the ones they were taken from. Anything else (a
 * document the view was given later) is diffed afresh.
 */
export function overrideFor(sides: FileSides): (a: string, b: string) => readonly Change[] {
  const changes = mergeChanges(sides)
  return (a, b) => {
    if (a !== sides.a || b !== sides.b) return presentableDiff(a, b)
    // A copy: the merge view tidies the list it is given in place.
    return [...changes]
  }
}
