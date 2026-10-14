import { expect, test } from 'bun:test'
import { acceptsUploads, blankForm, buildLine, canonical, lines, problems, replayRepos, repoEdit, repoOps, toEntry, toForm, words, writtenRepos } from './repos'
import type { RepoEntry, RepoImageBuild } from './types'

const presets: [string, string[]][] = [
  ['crates', ['crates.io', 'index.crates.io', 'static.crates.io']],
  ['npm', ['registry.npmjs.org']],
  ['packagist', ['packagist.org', 'repo.packagist.org', 'api.github.com', 'codeload.github.com']],
  ['github', ['github.com', 'api.github.com', 'codeload.github.com']],
]

test('an entry survives the form unchanged', () => {
  const entries: RepoEntry[] = [
    {
      path: '/src/app',
      dockerfile: '.devcontainer/Dockerfile',
      context: '.',
      checks: ['just check', 'just test'],
      timeout_secs: 1800,
      prepare: ['cargo fetch --locked', 'cd spa && bun install --frozen-lockfile'],
      egress: ['crates', 'npm'],
      session_egress: true,
    },
    { path: 'github.com/owner/notes', checks: [], prepare: [], egress: [] },
    { path: 'owner/pinned', image: 'localhost/tc@sha256:abc', prepare: [], egress: ['example.org'] },
    { path: '/src/plain', prepare: [], egress: [] },
  ]
  for (const entry of entries) expect(toEntry(toForm(entry))).toEqual(entry)
})

test('what the form does not edit is saved back as it came', () => {
  const entry: RepoEntry = {
    path: 'github.com/owner/app',
    prepare: [],
    egress: [],
    commits: 'keep',
    style: { conventional: true, max_subject: 72 },
  }
  const form = toForm(entry)
  form.prepare = 'bun install'
  expect(toEntry(form)).toEqual({ ...entry, prepare: ['bun install'] })
})

test('a table read back from the node is not an unsaved change', () => {
  // serde_json's map is sorted, so the node's keys never arrive in the order
  // the form writes them.
  const fromNode = [
    {
      checks: ['just check'],
      commits: 'squash',
      dockerfile: '.devcontainer/Dockerfile',
      egress: ['crates'],
      path: '/src/app',
      prepare: ['cargo fetch --locked'],
      session_egress: true,
      style: { conventional: true },
      timeout_secs: 900,
    },
    { egress: [], image: 'localhost/tc@sha256:abc', path: 'owner/pinned', prepare: [] },
  ] as RepoEntry[]
  const forms = fromNode.map(toForm)
  // Compared as it arrived, an untouched form would read as edited.
  expect(JSON.stringify(forms.map(toEntry))).not.toBe(JSON.stringify(fromNode))
  expect(JSON.stringify(forms.map(toEntry))).toBe(canonical(fromNode))
})

test('explicitly no checks is not the same as the node-wide checks', () => {
  const inherited = toForm({ path: '/a', prepare: [], egress: [] })
  expect(inherited.ownChecks).toBe(false)
  expect(toEntry(inherited).checks).toBeUndefined()
  const none = toForm({ path: '/a', checks: [], prepare: [], egress: [] })
  expect(none.ownChecks).toBe(true)
  expect(toEntry(none).checks).toEqual([])
})

test('a source the operator moved away from is not sent', () => {
  const form = { ...blankForm(), path: '/a', image: 'localhost/tc@sha256:abc', context: 'docker' }
  // Built from a Dockerfile: the image typed earlier must not ride along.
  expect(toEntry(form)).toEqual({ path: '/a', dockerfile: '.devcontainer/Dockerfile', context: 'docker', prepare: [], egress: [] })
  expect(toEntry({ ...form, source: 'image' })).toEqual({ path: '/a', image: 'localhost/tc@sha256:abc', prepare: [], egress: [] })
  expect(toEntry({ ...form, source: 'harness' })).toEqual({ path: '/a', prepare: [], egress: [] })
})

test('commands are one per line and egress is separated however it was typed', () => {
  expect(lines(' just check \n\n  just test\n')).toEqual(['just check', 'just test'])
  expect(words('crates, npm  example.org\npypi')).toEqual(['crates', 'npm', 'example.org', 'pypi'])
  expect(toEntry({ ...blankForm(), path: '/a', timeout: 'soon' }).timeout_secs).toBeUndefined()
  expect(toEntry({ ...blankForm(), path: '/a', timeout: '1800.9' }).timeout_secs).toBe(1800)
})

test('what the node would refuse is said first', () => {
  const ok = { ...blankForm(), path: '/a' }
  expect(problems([ok])).toEqual([])
  expect(problems([{ ...ok, path: ' ' }])).toEqual(['Every entry needs the repository it is for.'])
  expect(problems([ok, { ...ok }])[0]).toContain('named twice')
  expect(problems([{ ...ok, source: 'image' }])[0]).toContain('name the image')
  expect(problems([{ ...ok, dockerfile: '' }])[0]).toContain('name the Dockerfile')
  expect(problems([{ ...ok, sessionEgress: true }])[0]).toContain('its egress is empty')
  expect(problems([{ ...ok, sessionEgress: true, egress: 'crates' }])).toEqual([])
})

test('the entries that reach somewhere an upload could go are named', () => {
  expect(acceptsUploads(['crates', 'npm'], presets)).toEqual([])
  expect(acceptsUploads(['crates', 'packagist', 'github', 'api.github.com', 'example.org'], presets)).toEqual([
    'packagist',
    'github',
    'api.github.com',
  ])
})

test('a build reads as what it is and how it stands', () => {
  const build: RepoImageBuild = {
    id: '1',
    repo_path: '/src/app',
    kind: 'base',
    source_ref: 'origin/main',
    source_commit: '5a2bcd4d15b3379e4b55',
    image: 'localhost/tracon-repo-app@sha256:abc',
    status: 'ready',
    log_tail: '',
    error: '',
    warnings: [],
    started_ms: 1,
    finished_ms: 2,
  }
  expect(buildLine(build)).toBe('image from origin/main 5a2bcd4d15b3 · ready')
  expect(buildLine({ ...build, status: 'building' })).toBe('image from origin/main 5a2bcd4d15b3 · building…')
  expect(buildLine({ ...build, status: 'failed', error: 'podman build exited 1' })).toBe(
    'image from origin/main 5a2bcd4d15b3 · failed: podman build exited 1',
  )
  expect(buildLine({ ...build, kind: 'session:claude', source_ref: '', source_commit: '' })).toBe(
    'session image (claude) · ready',
  )
  expect(buildLine({ ...build, kind: 'trial' })).toBe('image tried by a session from origin/main 5a2bcd4d15b3 · ready')
})

test('a repository save is per-entry operations, and an untouched table sends none', () => {
  const seeded: RepoEntry[] = [
    { path: '/src/a', dockerfile: '.devcontainer/Dockerfile', prepare: ['cargo fetch'], egress: ['crates'], commits: 'keep', style: { conventional: true } },
    { path: '/src/b', prepare: [], egress: [] },
    // Written with what the form does not say (a zero timeout is the default).
    { path: '/src/c', prepare: [], egress: [], timeout_secs: 0 },
  ]
  const forms = () => seeded.map((entry, origin) => ({ ...toForm(entry), origin }))
  expect(repoOps(seeded, forms())).toEqual([])

  // One field: one set, at its index; what the form does not edit is untouched.
  const egress = forms()
  egress[0].egress = 'crates npm'
  egress[1].sessionEgress = false
  expect(repoOps(seeded, egress)).toEqual([{ op: 'set', key: 'repo[0].egress', value: ['crates', 'npm'] }])

  // Switching a source takes the old key out and writes the new one.
  const pinned = forms()
  pinned[0].source = 'image'
  pinned[0].image = 'localhost/tc@sha256:abc'
  expect(repoOps(seeded, pinned)).toEqual([
    { op: 'remove', key: 'repo[0].dockerfile' },
    { op: 'set', key: 'repo[0].image', value: 'localhost/tc@sha256:abc' },
  ])

  // Remove b, add d first, keep a and c: removal, insert, then the order.
  const all = forms()
  const fresh = { ...blankForm(), path: '/src/d', source: 'harness' as const }
  expect(repoOps(seeded, [fresh, all[2], all[0]])).toEqual([
    { op: 'remove', key: 'repo[1]' },
    { op: 'insert', array: 'repo', entry: { path: '/src/d', prepare: [], egress: [] } },
    { op: 'reorder', array: 'repo', order: [2, 1, 0] },
  ])

  // Moving an entry is a reorder alone.
  const swapped = forms()
  expect(repoOps(seeded, [swapped[1], swapped[0], swapped[2]])).toEqual([{ op: 'reorder', array: 'repo', order: [1, 0, 2] }])
})

test('the table is read as node.toml writes it, with the lists it may leave out empty', () => {
  expect(writtenRepos({})).toEqual([])
  expect(writtenRepos({ repo: [{ path: '/src/a', egress: ['crates'] }] })).toEqual([{ path: '/src/a', prepare: [], egress: ['crates'] }])
})

test('keeping my edits after the table moved replays only my changes over what the node has now', () => {
  const seeded: RepoEntry[] = [
    { path: '/src/a', prepare: ['cargo fetch'], egress: ['crates'] },
    { path: '/src/b', prepare: [], egress: [] },
    { path: '/src/c', prepare: [], egress: [] },
  ]
  const forms = seeded.map((entry, origin) => ({ ...toForm(entry), origin }))
  // Mine: a's preparation, c removed, a new d.
  forms[0].prepare = 'cargo fetch --locked'
  const mine = [forms[0], forms[1], { ...blankForm(), path: '/src/d', source: 'harness' as const }]
  // Theirs: a's egress widened, b removed, e added first.
  const now: RepoEntry[] = [
    { path: '/src/e', prepare: [], egress: ['npm'] },
    { path: '/src/a', prepare: ['cargo fetch'], egress: ['crates', 'npm'] },
    { path: '/src/c', prepare: [], egress: [] },
  ]
  const out = replayRepos(seeded, mine, now)
  expect(out.removed).toEqual(['/src/b'])
  expect(out.clashes).toEqual([])
  expect(out.forms.map((f) => [f.path, f.origin])).toEqual([
    ['/src/e', 0],
    ['/src/a', 1],
    ['/src/d', null],
  ])
  const a = toEntry(out.forms[1])
  expect(a.prepare).toEqual(['cargo fetch --locked'])
  expect(a.egress).toEqual(['crates', 'npm'])
  // Saving that: c (which I removed) goes, e and a's new egress stay, d is added.
  expect(repoOps(now, out.forms)).toEqual([
    { op: 'remove', key: 'repo[2]' },
    { op: 'set', key: 'repo[1].prepare', value: ['cargo fetch --locked'] },
    { op: 'insert', array: 'repo', entry: { path: '/src/d', prepare: [], egress: [] } },
  ])
  // A field both changed is named; mine is what saving writes.
  const both = replayRepos(seeded, [forms[0]], [{ path: '/src/a', prepare: ['make deps'], egress: ['crates'] }])
  expect(both.clashes).toEqual(['/src/a: prepare'])
  expect(toEntry(both.forms[0]).prepare).toEqual(['cargo fetch --locked'])
})

test('a refusal of a repository field lands on the form it came from', () => {
  const seeded: RepoEntry[] = [
    { path: '/src/a', prepare: [], egress: [] },
    { path: '/src/b', prepare: [], egress: [] },
  ]
  const forms = seeded.map((entry, origin) => ({ ...toForm(entry), origin }))
  forms[1].egress = 'npm'
  const { ops, screen } = repoEdit(seeded, [forms[1]])
  expect(ops).toEqual([
    { op: 'remove', key: 'repo[0]' },
    { op: 'set', key: 'repo[0].egress', value: ['npm'] },
  ])
  expect(screen).toEqual([undefined, 'repo[0].egress'])
})
