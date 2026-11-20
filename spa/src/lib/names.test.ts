import { expect, test } from 'bun:test'
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'

import vectors from '../../../spec/vectors/names.json'
import {
  applyOperatorName,
  DOMAIN,
  generatedName,
  looksLikeId,
  nameHash,
  nameOf,
  nameProblem,
  nameWords,
  nodeDisplayName,
  operatorName,
  setOperatorNames,
  type NameKind,
} from './names'

test('matches the spec vectors the node is pinned by', () => {
  expect(vectors.domain).toBe(DOMAIN)
  expect(nameWords.adjectives.length).toBe(vectors.adjectives)
  expect(nameWords.nouns.length).toBe(vectors.nouns)
  for (const v of vectors.vectors) {
    expect(nameHash(v.kind, v.id).toString(16).padStart(16, '0')).toBe(v.hash_hex)
    expect(generatedName(v.kind as NameKind, v.id)).toBe(v.name)
  }
})

test('two UUIDv7s minted in the same millisecond get different names', () => {
  const a = '019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d'
  const b = '019a9e5c-7f20-7f00-b123-456789abcdef'
  expect(a.slice(0, 13)).toBe(b.slice(0, 13))
  expect(generatedName('session', a)).not.toBe(generatedName('session', b))
  expect(generatedName('session', a)).toMatch(/^session-[a-z]{3,8}-[a-z]{3,8}-\d\d$/)
})

test('a node reads by its own name when known and by its generated one when not', () => {
  const id = '3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c'
  expect(nodeDisplayName([{ id, name: 'bazzite' }], id)).toBe('bazzite')
  expect(nodeDisplayName([], id)).toBe(generatedName('node', id))
  expect(nodeDisplayName([{ id, name: '' }], id)).toMatch(/^node-/)
})

// No raw id prefix is shown anywhere: records read by name (`Name.svelte`,
// `generatedName`). What may still be cut short is what the issue left out of
// scope: git commit SHAs (and the build's commit), content hashes through
// `shortHash`, and slices that are not ids at all. Each is allowed by the
// expression it cuts, so a new id prefix fails here whatever file it is in.
test('no id prefix is rendered outside the out-of-scope rows', () => {
  const root = new URL('..', import.meta.url).pathname
  const files: string[] = []
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir)) {
      const path = join(dir, entry)
      if (statSync(path).isDirectory()) walk(path)
      else if (/\.(ts|svelte)$/.test(entry) && !entry.endsWith('.test.ts')) files.push(path)
    }
  }
  walk(root)
  // `.slice(0, n)` or `.slice(-n)` for the lengths an id prefix is cut to, and
  // the expression it cuts: a path such as `item.head_sha`, possibly a call.
  const cut = /\.(?:slice|substring)\((?:0, ?(?:4|6|8|12|16)|-(?:4|6|8|12))\)/g
  const receiverOf = (before: string) => /[\w.?]*(?:\([^()]*\))?$/.exec(before)?.[0] ?? ''
  const allowed = [
    /(?:^|\.)(?:head_sha|sha|source_commit|commit|lease\??|build)$/, // git commits
    /^(?:String|str)\((?:e\.payload|p)\.head_sha[^()]*\)$/, // a git commit, coerced
    /^from$/, // DirectPush: the commit a fast-forward starts from
    /^hash$/, // shortHash itself
    /\.toISOString\(\)$/, // a datetime-local value
    /^store\.queue\.ended$/, // a list
    /\.split\('\\n'\)$/, // the end of a log
  ]
  const offenders: string[] = []
  for (const file of files) {
    readFileSync(file, 'utf8')
      .split('\n')
      .forEach((line, i) => {
        for (const m of line.matchAll(cut)) {
          const receiver = receiverOf(line.slice(0, m.index))
          if (!allowed.some((rule) => rule.test(receiver))) offenders.push(`${file.slice(root.length)}:${i + 1}: ${receiver}${m[0]}`)
        }
      })
  }
  expect(offenders).toEqual([])
})

test("an operator's name reads in place of the generated one, and a revert puts it back", () => {
  const id = '019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d'
  setOperatorNames([{ kind: 'session', id, name: 'Login flow' }])
  expect(nameOf('session', id)).toBe('Login flow')
  // Another kind of the same id is not named by it.
  expect(nameOf('approval', id)).toBe(generatedName('approval', id))
  applyOperatorName('session', id, null)
  expect(operatorName('session', id)).toBeUndefined()
  expect(nameOf('session', id)).toBe(generatedName('session', id))
  // A review and a report share a page: a report's name is found either way.
  applyOperatorName('report', id, 'Weekly notes')
  expect(nameOf('review', id)).toBe('Weekly notes')
  // A fresh list drops names the node no longer has.
  setOperatorNames([])
  expect(operatorName('report', id)).toBeUndefined()
})

test('a name is checked as the node checks it', () => {
  expect(nameProblem('Login flow')).toBeNull()
  expect(nameProblem('cafe')).toBeNull()
  expect(nameProblem('  ')).not.toBeNull()
  expect(nameProblem('x'.repeat(65))).not.toBeNull()
  expect(nameProblem('two\nlines')).not.toBeNull()
  expect(nameProblem('deadbeef')).not.toBeNull()
  expect(nameProblem('019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d')).not.toBeNull()
})

test('an id reads as an id, and a name or a prefix does not', () => {
  expect(looksLikeId('019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d')).toBe(true)
  expect(looksLikeId('ab'.repeat(32))).toBe(true)
  expect(looksLikeId('019a9e5c')).toBe(false)
  expect(looksLikeId('session-loving-bubble-21')).toBe(false)
})
