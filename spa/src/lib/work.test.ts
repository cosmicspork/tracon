import { expect, test } from 'bun:test'

import type { Project, WorkView } from './types'
import { inProject, isTopPriority, matches, priorityLabel, projectName, projectTabs } from './work'

const levels = [
  { name: 'Urgent', value: 4 },
  { name: 'Priority', value: 3 },
  { name: 'Planned', value: 2 },
  { name: 'Backlog', value: 1 },
]

const view = (o: Partial<WorkView>): WorkView => ({
  id: 'aaaaaaaa11',
  channel: 'personal',
  project_id: null,
  title: 't',
  body: '',
  state: 'open',
  priority: 0,
  deps: [],
  discovered_from: null,
  discovered_by_session: null,
  phase_plan_slug: null,
  brief_slug: null,
  closed_by_session: null,
  created_ms: 0,
  updated_ms: 0,
  readiness: { state: 'ready' },
  session_id: null,
  ...o,
})

const project = (id: string, name: string): Project => ({ id, name, remote_url: null, open: 0, total: 0 })

test('a priority reads as its level, or as its number when no level has it', () => {
  expect(priorityLabel(4, levels)).toBe('Urgent')
  expect(priorityLabel(0, levels)).toBe('0')
  expect(priorityLabel(3, [])).toBe('3')
  expect(isTopPriority(4, levels)).toBe(true)
  expect(isTopPriority(9, levels)).toBe(true)
  expect(isTopPriority(3, levels)).toBe(false)
  expect(isTopPriority(9, [])).toBe(false)
})

test('a search wants every word, in the title or body, or the start of the id', () => {
  const item = { id: '0b0e29c44d', title: 'Word Machine drops lists', body: 'Past the first 100' }
  expect(matches(item, '')).toBe(true)
  expect(matches(item, 'machine LISTS')).toBe(true)
  expect(matches(item, 'machine 100')).toBe(true)
  expect(matches(item, 'machine authored')).toBe(false)
  expect(matches(item, '0b0e29')).toBe(true)
  expect(matches(item, 'e29c')).toBe(false)
})

test('tabs are All, each project that holds an item, an unnamed one by id, then No project', () => {
  const items = [
    view({ id: 'a1', project_id: 'p-hdr' }),
    view({ id: 'a2', project_id: 'p-hdr', readiness: { state: 'closed' }, state: 'closed' }),
    view({ id: 'a3', project_id: 'p-tracon' }),
    view({ id: 'a4', project_id: 'feedc0ffee12' }),
    view({ id: 'a5' }),
  ]
  const projects = [project('p-hdr', 'hounddogreading'), project('p-tracon', 'tracon'), project('p-idle', 'idle')]
  const tabs = projectTabs(items, projects)
  expect(tabs.map((t) => [t.label, t.open])).toEqual([
    ['All', 4],
    ['hounddogreading', 1],
    ['tracon', 1],
    ['feedc0ff', 1],
    ['No project', 1],
  ])
  expect(projectTabs(items.slice(0, 1), projects).map((t) => t.key)).toEqual(['all', 'p-hdr'])
  expect(inProject(items[4], 'none')).toBe(true)
  expect(inProject(items[0], 'none')).toBe(false)
  expect(inProject(items[0], 'all')).toBe(true)
  expect(projectName(items[3], projects)).toBe('feedc0ff')
  expect(projectName(items[4], projects)).toBeNull()
})
