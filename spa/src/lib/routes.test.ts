import { expect, test } from 'bun:test'
import { localDate, redirect, screenFor, settingsSection, tasksSearch, tasksView, usageSearch, usageView, usageWindow } from './routes'

test('old top-level addresses lead to where the screen moved', () => {
  expect(redirect('/metrics')).toBe('/usage')
  expect(redirect('/work')).toBe('/tasks')
  expect(redirect('/nodes')).toBe('/settings#mesh')
  // Evidence is gone: a bookmark to it, or to one of its candidates, opens the tasks.
  expect(redirect('/evidence')).toBe('/tasks')
  expect(redirect('/evidence', '?candidate=abc%3Apersonal')).toBe('/tasks')
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

test('the Usage address reads a span of days, a date range, a provider and a grouping', () => {
  expect(usageView('')).toEqual({ days: 30, from: null, to: null, provider: null, group: 'model' })
  expect(usageView('?since=7')).toEqual({ days: 7, from: null, to: null, provider: null, group: 'model' })
  expect(usageView('?since=2026-10-01&until=2026-10-15&provider=openai-codex&group=day')).toEqual({
    days: null,
    from: '2026-10-01',
    to: '2026-10-15',
    provider: 'openai-codex',
    group: 'day',
  })
  // A start after the end reads as the same days, swapped.
  expect(usageView('?since=2026-10-15&until=2026-10-01')).toMatchObject({ from: '2026-10-01', to: '2026-10-15' })
  // Unreadable parts fall back rather than emptying the page.
  expect(usageView('?since=soon&until=2026-02-30&group=week&provider=')).toEqual({
    days: 30,
    from: null,
    to: null,
    provider: null,
    group: 'model',
  })
})

test('a Usage view writes back the address it was read from, defaults left out', () => {
  for (const search of ['', '?since=7', '?since=2026-10-01&until=2026-10-15&provider=openai-codex&group=session'])
    expect(usageSearch(usageView(search))).toBe(search)
  expect(usageSearch(usageView('?since=30&group=model'))).toBe('')
})

test('a Usage window runs from the first local day to the end of the last', () => {
  const day = (s: string) => new Date(`${s}T00:00:00`).getTime()
  expect(usageWindow(usageView('?since=2026-10-01&until=2026-10-15'))).toEqual({
    since: day('2026-10-01'),
    until: day('2026-10-16'),
  })
  const now = day('2026-10-21') + 5_000
  expect(usageWindow(usageView('?since=7'), now)).toEqual({ since: now - 7 * 86_400_000, until: null })
  expect(localDate(day('2026-10-21') + 1)).toBe('2026-10-21')
})

test('each address opens its screen, the first match winning', () => {
  expect(screenFor('/')).toEqual({ name: 'home' })
  expect(screenFor('/sessions')).toEqual({ name: 'sessions' })
  expect(screenFor('/sessions/s-1')).toEqual({ name: 'session', id: 's-1' })
  expect(screenFor('/reviews/r-1/files')).toEqual({ name: 'review', id: 'r-1' })
  expect(screenFor('/tasks/wi-2')).toEqual({ name: 'task', id: 'wi-2' })
  expect(screenFor('/nodes/enroll')).toEqual({ name: 'enroll' })
  expect(screenFor('/docs/personal/guide-x')).toEqual({ name: 'doc', channel: 'personal', slug: 'guide-x', edit: false })
  expect(screenFor('/docs/personal/guide-x/edit')).toMatchObject({ name: 'doc', edit: true })
  expect(screenFor('/docs/personal/guide-x/preview')).toEqual({ name: 'doc-preview', channel: 'personal', slug: 'guide-x' })
  expect(screenFor('/docs')).toEqual({ name: 'docs' })
  expect(screenFor('/memories')).toEqual({ name: 'memories' })
  // Nothing renders at an address no screen claims but the home.
  expect(screenFor('/sessions/')).toEqual({ name: 'home' })
  expect(screenFor('/nowhere')).toEqual({ name: 'home' })
})

test('a Settings anchor opens its section, and anything else General', () => {
  expect(settingsSection('#mesh')).toBe('mesh')
  expect(settingsSection('')).toBe('general')
  expect(settingsSection('#nonsense')).toBe('general')
})

test('the Tasks address carries the project tab and the search, and nothing that is the default', () => {
  expect(tasksView('')).toEqual({ project: 'all', q: '' })
  expect(tasksView('?project=p1&q=word%20machine')).toEqual({ project: 'p1', q: 'word machine' })
  expect(tasksSearch({ project: 'all', q: '' })).toBe('')
  expect(tasksSearch({ project: 'none', q: '  ' })).toBe('?project=none')
  const view = { project: 'p1', q: 'word machine' }
  expect(tasksView(tasksSearch(view))).toEqual(view)
})
