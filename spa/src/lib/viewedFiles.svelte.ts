// Which files of a review the reader has marked viewed: their own progress
// through a large change, and nothing more. Viewed is not approved; nothing
// here is sent to the node or the agent, and no verdict reads it.
//
// A mark is bound to what the reader saw: the revision it was made on and the
// file's diff as that revision showed it. When the agent resubmits, a file
// whose diff is the same carries its mark over to the new revision; a file
// whose diff changed loses it and says it changed since it was viewed.
//
// Marks are kept on this device, by review, like the diff layout.

import type { DiffFile } from './diff'

/** One file's mark: the revision it was made on and the file's diff then. */
export interface Mark {
  revision: string
  hash: string
}

export type ViewedState = 'viewed' | 'changed' | 'unviewed'

export const viewedKey = (reviewId: string) => `tracon-viewed-${reviewId}`

/**
 * A fingerprint of one file's change: its diff lines, without what moves
 * when only the base moves under it (the blob hashes on the `index` line and
 * the line numbers on each hunk header). A binary file shows no lines, so its
 * `index` line is the only thing that says its contents changed, and is kept.
 */
export function fileHash(file: DiffFile): string {
  const hunks = file.lines.some((l) => l.startsWith('@@'))
  let h1 = 0xdeadbeef
  let h2 = 0x41c6ce57
  for (const raw of file.lines) {
    if (hunks && raw.startsWith('index ')) continue
    const line = raw.startsWith('@@') ? raw.replace(/^@@[^@]*@@/, '@@') : raw
    for (let i = 0; i <= line.length; i++) {
      const c = i < line.length ? line.charCodeAt(i) : 10
      h1 = Math.imul(h1 ^ c, 2654435761)
      h2 = Math.imul(h2 ^ c, 1597334677)
    }
  }
  h1 = Math.imul(h1 ^ (h1 >>> 16), 2246822507) ^ Math.imul(h2 ^ (h2 >>> 13), 3266489909)
  h2 = Math.imul(h2 ^ (h2 >>> 16), 2246822507) ^ Math.imul(h1 ^ (h1 >>> 13), 3266489909)
  return (4294967296 * (2097151 & h2) + (h1 >>> 0)).toString(36)
}

/** A mark's meaning for a file whose diff now fingerprints as `hash`. */
export function stateOf(mark: Mark | undefined, hash: string): ViewedState {
  if (!mark) return 'unviewed'
  return mark.hash === hash ? 'viewed' : 'changed'
}

type Storage_ = Pick<Storage, 'getItem' | 'setItem'>

/** The marks kept for a review, or none when storage cannot say. */
export function loadMarks(reviewId: string, storage?: Storage_): Record<string, Mark> {
  try {
    const raw = (storage ?? globalThis.localStorage)?.getItem(viewedKey(reviewId))
    const parsed = raw ? JSON.parse(raw) : {}
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? parsed : {}
  } catch {
    return {}
  }
}

export function saveMarks(reviewId: string, marks: Record<string, Mark>, storage?: Storage_) {
  try {
    ;(storage ?? globalThis.localStorage)?.setItem(viewedKey(reviewId), JSON.stringify(marks))
  } catch {
    /* blocked storage: the marks last this page */
  }
}

/** The reader's viewed marks on one revision of a review. */
export class ViewedFiles {
  #reviewId: string
  #revision: string
  #storage: Storage_ | undefined
  /** Each file of the revision's full diff, by path, as its fingerprint. */
  #hashes: Map<string, string>
  marks = $state<Record<string, Mark>>({})

  constructor(reviewId: string, revision: string, files: DiffFile[], storage?: Storage_) {
    this.#reviewId = reviewId
    this.#revision = revision
    this.#storage = storage
    this.#hashes = new Map(files.map((f) => [f.path, fileHash(f)]))
    this.marks = loadMarks(reviewId, storage)
  }

  /** Whether a file can be marked: it is one of the revision's files. */
  tracks(path: string): boolean {
    return this.#hashes.has(path)
  }

  state(path: string): ViewedState {
    const hash = this.#hashes.get(path)
    return hash === undefined ? 'unviewed' : stateOf(this.marks[path], hash)
  }

  /** Mark a file viewed on this revision, or take the mark off. */
  set(path: string, on: boolean) {
    const hash = this.#hashes.get(path)
    if (hash === undefined) return
    const next = { ...this.marks }
    if (on) next[path] = { revision: this.#revision, hash }
    else delete next[path]
    this.marks = next
    saveMarks(this.#reviewId, next, this.#storage)
  }

  /** How many of the revision's files are viewed, of how many. */
  get progress(): { viewed: number; of: number } {
    let viewed = 0
    for (const path of this.#hashes.keys()) if (this.state(path) === 'viewed') viewed++
    return { viewed, of: this.#hashes.size }
  }
}
