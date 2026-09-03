// The patch these build is handed to an agent to `git apply`, so the test
// that matters is whether git accepts it and produces the intended file.
// Everything here goes through real git in a temporary repository.

import { expect, test, describe } from 'bun:test'
import { mkdtempSync, writeFileSync, readFileSync, rmSync, mkdirSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { spawnSync } from 'node:child_process'

import { unifiedDiff, buildPatch, fileSection, baseFromDiff } from './patch'

/**
 * Write `before`, apply the generated patch with git, return the result.
 *
 * `into` reuses an existing repository. The fuzz round below passes one,
 * because a temp directory and a `git init` per round is several hundred extra
 * subprocesses, and a single transient spawn failure among them reads exactly
 * like a rejected patch — which is how this test failed in CI once, with an
 * empty stderr and a patch that git accepts perfectly well by hand.
 */
function applied(files: Array<{ path: string; before: string; after: string }>, into?: string) {
  const dir = into ?? mkdtempSync(join(tmpdir(), 'tracon-patch-'))
  try {
    if (!into) spawnSync('git', ['init', '-q'], { cwd: dir })
    for (const f of files) {
      mkdirSync(join(dir, dirname(f.path)), { recursive: true })
      writeFileSync(join(dir, f.path), f.before)
    }
    const patch = buildPatch(files)
    writeFileSync(join(dir, 'edit.patch'), patch)
    const res = spawnSync('git', ['apply', '--verbose', 'edit.patch'], { cwd: dir, encoding: 'utf8' })
    // Read everything back before the directory goes away.
    const contents = new Map<string, string>()
    for (const f of files) {
      try {
        contents.set(f.path, readFileSync(join(dir, f.path), 'utf8'))
      } catch {
        contents.set(f.path, '<<missing>>')
      }
    }
    return {
      ok: res.status === 0,
      // Say why, including the cases where git never ran: an empty stderr with
      // a valid patch is not a rejection and should not read like one.
      stderr:
        res.stderr ||
        (res.error ? `spawn failed: ${res.error.message}` : '') ||
        (res.signal ? `killed by ${res.signal}` : '') ||
        (res.status === 0 ? '' : `git exited ${res.status} with no message`),
      patch,
      read: (p: string) => contents.get(p) ?? '<<missing>>',
    }
  } finally {
    if (!into) rmSync(dir, { recursive: true, force: true })
  }
}

/** One file, edited: git must accept the patch and land exactly `after`. */
function roundTrip(before: string, after: string, path = 'src/a.ts') {
  const r = applied([{ path, before, after }])
  expect(r.ok, `git rejected the patch:\n${r.stderr}\n---\n${r.patch}`).toBe(true)
  expect(r.read(path)).toBe(after)
  return r
}

describe('git applies what we generate', () => {
  test('a line changed in the middle', () => {
    const before = ['one', 'two', 'three', 'four', 'five', 'six', 'seven', 'eight'].join('\n') + '\n'
    const after = before.replace('five', 'FIVE')
    roundTrip(before, after)
  })

  test('a line inserted', () => {
    const before = ['a', 'b', 'c', 'd', 'e', 'f'].join('\n') + '\n'
    const after = ['a', 'b', 'c', 'NEW', 'd', 'e', 'f'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('a line deleted', () => {
    const before = ['a', 'b', 'c', 'd', 'e', 'f'].join('\n') + '\n'
    const after = ['a', 'b', 'd', 'e', 'f'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('changes far apart become separate hunks', () => {
    const before = Array.from({ length: 40 }, (_, i) => `line ${i}`).join('\n') + '\n'
    const after = before.replace('line 2\n', 'line TWO\n').replace('line 35\n', 'line THIRTY-FIVE\n')
    const r = roundTrip(before, after)
    expect(r.patch.match(/^@@/gm)?.length).toBe(2)
  })

  test('changes close together stay one hunk', () => {
    const before = Array.from({ length: 20 }, (_, i) => `line ${i}`).join('\n') + '\n'
    const after = before.replace('line 8\n', 'line EIGHT\n').replace('line 9\n', 'line NINE\n')
    const r = roundTrip(before, after)
    expect(r.patch.match(/^@@/gm)?.length).toBe(1)
  })

  test('a change at the very start', () => {
    const before = ['first', 'second', 'third', 'fourth'].join('\n') + '\n'
    const after = ['FIRST', 'second', 'third', 'fourth'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('a change at the very end', () => {
    const before = ['first', 'second', 'third', 'fourth'].join('\n') + '\n'
    const after = ['first', 'second', 'third', 'FOURTH'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('lines appended past the end', () => {
    const before = ['a', 'b'].join('\n') + '\n'
    const after = ['a', 'b', 'c', 'd'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('every line replaced', () => {
    const before = ['a', 'b', 'c'].join('\n') + '\n'
    const after = ['x', 'y', 'z'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('the whole file emptied', () => {
    roundTrip('a\nb\nc\n', '')
  })

  test('a file with no trailing newline', () => {
    roundTrip('alpha\nbeta', 'alpha\nBETA')
  })

  test('a trailing newline added', () => {
    roundTrip('alpha\nbeta', 'alpha\nbeta\n')
  })

  test('duplicate lines are not confused for one another', () => {
    const before = ['x', 'same', 'y', 'same', 'z', 'same', 'w'].join('\n') + '\n'
    const after = ['x', 'same', 'y', 'CHANGED', 'z', 'same', 'w'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('indentation-only edits survive', () => {
    const before = ['fn main() {', 'let x = 1;', '}'].join('\n') + '\n'
    const after = ['fn main() {', '    let x = 1;', '}'].join('\n') + '\n'
    roundTrip(before, after)
  })

  test('several files in one patch', () => {
    const files = [
      { path: 'a.txt', before: 'one\ntwo\nthree\n', after: 'one\nTWO\nthree\n' },
      { path: 'nested/b.txt', before: 'alpha\nbeta\n', after: 'alpha\nbeta\ngamma\n' },
    ]
    const r = applied(files)
    expect(r.ok, `git rejected the patch:\n${r.stderr}\n---\n${r.patch}`).toBe(true)
    expect(r.read('a.txt')).toBe(files[0].after)
    expect(r.read('nested/b.txt')).toBe(files[1].after)
  })

  test('a long realistic edit', () => {
    const before = `export function greet(name: string) {
  const greeting = 'hello'
  if (!name) {
    return greeting
  }
  return greeting + ', ' + name
}

export function farewell(name: string) {
  return 'bye, ' + name
}
`
    const after = `export function greet(name: string) {
  const greeting = 'hello'
  if (!name) {
    throw new Error('a greeting needs somebody to greet')
  }
  return \`\${greeting}, \${name}\`
}

export function farewell(name: string) {
  if (!name) throw new Error('same here')
  return \`bye, \${name}\`
}
`
    roundTrip(before, after, 'src/greet.ts')
  })
})

describe('what it declines to produce', () => {
  test('an unchanged file yields no diff at all', () => {
    expect(unifiedDiff('a.ts', 'same\n', 'same\n')).toBeNull()
    expect(buildPatch([{ path: 'a.ts', before: 'x\n', after: 'x\n' }])).toBe('')
  })

  test('only the files actually edited appear', () => {
    const patch = buildPatch([
      { path: 'touched.ts', before: 'a\n', after: 'b\n' },
      { path: 'untouched.ts', before: 'a\n', after: 'a\n' },
    ])
    expect(patch).toContain('touched.ts')
    expect(patch).not.toContain('untouched.ts')
  })
})

/// Hand-picked cases cover what was thought of. This covers what was not:
/// random edits, checked by git rather than by eye.
test('git accepts randomly generated edits', () => {
  // A fixed sequence, so a failure is reproducible rather than a rumour.
  // One repository for every round; each round overwrites `f.txt` anyway.
  const dir = mkdtempSync(join(tmpdir(), 'tracon-fuzz-'))
  spawnSync('git', ['init', '-q'], { cwd: dir })
  let seed = 20260829
  const rand = () => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff
    return seed / 0x7fffffff
  }
  const pick = <T,>(xs: T[]) => xs[Math.floor(rand() * xs.length)]

  for (let round = 0; round < 400; round++) {
    const n = 1 + Math.floor(rand() * 25)
    // A small alphabet, so duplicate lines happen often — that is where a
    // line differ is most likely to go wrong.
    const source = Array.from({ length: n }, () => pick(['a', 'b', 'c', 'same', 'same', '']))
    const edited = [...source]
    const edits = 1 + Math.floor(rand() * 4)
    for (let e = 0; e < edits; e++) {
      if (edited.length === 0 || rand() < 0.4) {
        edited.splice(Math.floor(rand() * (edited.length + 1)), 0, pick(['new', 'x', 'same']))
      } else if (rand() < 0.5) {
        edited.splice(Math.floor(rand() * edited.length), 1)
      } else {
        edited[Math.floor(rand() * edited.length)] = pick(['CHANGED', 'y', 'same'])
      }
    }
    const nl = () => (rand() < 0.25 ? '' : '\n')
    const before = source.length ? source.join('\n') + nl() : ''
    const after = edited.length ? edited.join('\n') + nl() : ''
    if (before === after) continue
    const r = applied([{ path: 'f.txt', before, after }], dir)
    expect(
      r.ok,
      `round ${round}: git rejected\n${r.stderr}\nbefore=${JSON.stringify(before)}\nafter=${JSON.stringify(after)}\n${r.patch}`,
    ).toBe(true)
    expect(r.read('f.txt'), `round ${round}: wrong result for ${JSON.stringify(before)}`).toBe(after)
  }
  rmSync(dir, { recursive: true, force: true })
}, 60_000)

test('the header names the file on both sides', () => {
  const patch = unifiedDiff('src/deep/file.rs', 'a\n', 'b\n')!
  expect(patch.split('\n')[0]).toBe('--- a/src/deep/file.rs')
  expect(patch.split('\n')[1]).toBe('+++ b/src/deep/file.rs')
})

describe('rebuilding the base side from a diff', () => {
  /** A real git diff of base→head, as a review stores it. */
  function gitDiff(base: string, head: string, path = 'f.txt') {
    const dir = mkdtempSync(join(tmpdir(), 'tracon-base-'))
    try {
      spawnSync('git', ['init', '-q'], { cwd: dir })
      spawnSync('git', ['config', 'user.email', 'x@y.z'], { cwd: dir })
      spawnSync('git', ['config', 'user.name', 'x'], { cwd: dir })
      writeFileSync(join(dir, path), base)
      spawnSync('git', ['add', '-A'], { cwd: dir })
      spawnSync('git', ['commit', '-qm', 'base'], { cwd: dir })
      writeFileSync(join(dir, path), head)
      const res = spawnSync('git', ['diff', '--unified=3'], { cwd: dir, encoding: 'utf8' })
      return res.stdout
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  }

  test.each([
    ['a\nb\nc\nd\ne\n', 'a\nb\nCHANGED\nd\ne\n'],
    ['a\nb\n', 'a\nb\nc\nd\n'],
    ['a\nb\nc\nd\n', 'a\nd\n'],
    [Array.from({ length: 30 }, (_, i) => `l${i}`).join('\n') + '\n', Array.from({ length: 30 }, (_, i) => (i === 2 || i === 25 ? `L${i}` : `l${i}`)).join('\n') + '\n'],
    ['x\ny\n', 'x\ny\nz'],
    ['only\n', ''],
  ])('round-trips base %#', (base, head) => {
    const diff = gitDiff(base, head)
    const section = fileSection(diff, 'f.txt')
    expect(section, `no section for f.txt in:\n${diff}`).not.toBeNull()
    expect(baseFromDiff(head, section!)).toBe(base)
  })

  test('a diff that does not fit the text is refused rather than guessed at', () => {
    // Hunk claims to start past the end of a one-line file.
    const bogus = '@@ -1,3 +40,3 @@\n context\n-gone\n+new\n'
    expect(baseFromDiff('one line\n', bogus)).toBeNull()
  })
})
