import { expect, test } from 'bun:test'
import { defaultRepo, isManagedPath, managedFor, repoIdentity, repoLabel, repoMatches } from './repo'

test('the last segment names an ordinary checkout', () => {
  expect(repoLabel('/Users/you/src/project')).toBe('project')
  expect(repoLabel('/Users/you/src/project/')).toBe('project')
})

test('a managed clone is named by the directory above its repo', () => {
  expect(repoLabel('/var/lib/tracon/repos/github.com/cosmicspork/tracon/repo')).toBe('tracon')
})

test('the forge name wins when there is one', () => {
  expect(repoLabel('/var/lib/tracon/repos/github.com/cosmicspork/tracon/repo', 'cosmicspork/tracon')).toBe(
    'cosmicspork/tracon',
  )
  expect(repoLabel('/Users/you/src/project', '')).toBe('project')
  expect(repoLabel('/Users/you/src/project', null)).toBe('project')
})

test('a path that is only "repo" keeps what it has', () => {
  expect(repoLabel('repo')).toBe('repo')
  expect(repoLabel('/repo')).toBe('repo')
  expect(repoLabel('')).toBe('')
})

test('a repo search matches every word across the fields it is given', () => {
  expect(repoMatches('', 'anything')).toBe(true)
  expect(repoMatches('proj sub', 'group/sub/project', 'gitlab.example')).toBe(true)
  expect(repoMatches('Example', 'group/project', 'gitlab.example')).toBe(true)
  expect(repoMatches('other', 'group/project', null)).toBe(false)
})

test('a managed path is one the node cloned; anything else was typed', () => {
  const managed = [{ repo_path: '/state/repos/gitlab.example/g/p' }]
  expect(isManagedPath('/state/repos/gitlab.example/g/p', managed)).toBe(true)
  expect(isManagedPath('/home/op/src/p', managed)).toBe(false)
})

test('the default repository is the newest managed clone this node and channel used, by its identity', () => {
  const managed = [
    { repo_path: '/state/repos/github.com/o/a', host: 'github.com', full_name: 'o/a' },
    { repo_path: '/state/repos/github.com/o/b', host: 'github.com', full_name: 'o/b' },
  ]
  const s = (repo_path: string, created_ms: number, node_id = 'n1', channel = 'personal') => ({ node_id, channel, repo_path, created_ms })
  const sessions = [
    s('/state/repos/github.com/o/a', 1),
    s('/state/repos/github.com/o/b', 2),
    s('/private/tmp/validation/repo', 3),
    s('workspace://abc', 4),
    s('/state/repos/github.com/o/a', 5, 'n2'),
    s('/state/repos/github.com/o/a', 6, 'n1', 'work'),
  ]
  expect(defaultRepo(sessions, 'n1', 'personal', managed)).toBe('github.com/o/b')
  expect(defaultRepo([s('/private/tmp/validation/repo', 3)], 'n1', 'personal', managed)).toBeNull()
  expect(defaultRepo(sessions, 'n1', 'personal', [])).toBeNull()
})

test('a repository choice names its clone by identity or by the path a row records', () => {
  const clone = { repo_path: '/state/repos/gitlab.example/g/sub/p', host: 'gitlab.example', full_name: 'g/sub/p' }
  expect(repoIdentity(clone)).toBe('gitlab.example/g/sub/p')
  expect(managedFor('gitlab.example/g/sub/p', [clone])).toBe(clone)
  expect(managedFor('/state/repos/gitlab.example/g/sub/p', [clone])).toBe(clone)
  expect(managedFor('github.com/g/sub/p', [clone])).toBeNull()
  expect(repoLabel('github.com/cosmicspork/tracon')).toBe('tracon')
})
