import { expect, test } from 'bun:test'
import { redirect } from './routes'

test('old top-level addresses lead to where the screen moved', () => {
  expect(redirect('/metrics')).toBe('/usage')
  expect(redirect('/work')).toBe('/tasks')
  expect(redirect('/nodes')).toBe('/settings#mesh')
})

test('a task bookmarked under Work opens the same task', () => {
  expect(redirect('/work/wi-2')).toBe('/tasks/wi-2')
  expect(redirect('/work/wi-2', '?x=1', '#deps')).toBe('/tasks/wi-2?x=1#deps')
})

test('the query rides along; a Nodes hash gives way to the Mesh section', () => {
  expect(redirect('/metrics', '?since=7')).toBe('/usage?since=7')
  expect(redirect('/nodes', '?node=abc', '#x')).toBe('/settings?node=abc#mesh')
})

test('current addresses are left alone', () => {
  for (const path of ['/', '/tasks', '/tasks/wi-2', '/usage', '/sessions', '/settings', '/nodes/enroll', '/work/a/b'])
    expect(redirect(path)).toBeNull()
  expect(redirect('/settings', '', '#mesh')).toBeNull()
  expect(redirect('/settings', '', '#system')).toBeNull()
})

test('a Settings section folded into another opens the one that holds it now', () => {
  expect(redirect('/settings', '', '#maintenance')).toBe('/settings#system')
  expect(redirect('/settings', '', '#data')).toBe('/settings#system')
  expect(redirect('/settings', '', '#devices')).toBe('/settings#access')
  expect(redirect('/settings', '?node=abc', '#repositories')).toBe('/settings?node=abc#channels')
})

test('a section typed as a path opens as its anchor', () => {
  expect(redirect('/settings/maintenance')).toBe('/settings#system')
  expect(redirect('/settings/mesh/')).toBe('/settings#mesh')
})
