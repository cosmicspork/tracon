import { expect, test } from 'bun:test'
import { repoLabel } from './repo'

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
