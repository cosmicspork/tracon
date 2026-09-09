import { expect, test } from 'bun:test'
import { isManagedPath, repoLabel, repoMatches } from './repo'

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
