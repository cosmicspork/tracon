import { expect, test } from 'bun:test'
import { acceptsUploads, blankForm, buildLine, lines, problems, toEntry, toForm, words } from './repos'
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
})
