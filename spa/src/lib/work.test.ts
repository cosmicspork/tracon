import { expect, test } from 'bun:test'

import type { Project, WorkView } from './types'
import {
  inProject,
  isTopPriority,
  landing,
  landingLine,
  matches,
  taskName,
  pickableProjects,
  priorityLabel,
  projectLabel,
  projectName,
  projectTabs,
  workLabel,
  workState,
} from './work'

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
  const name = taskName(item.id)
  const [, adjective, noun] = name.split('-')
  expect(matches(item, name)).toBe(true)
  expect(matches(item, name.toUpperCase())).toBe(true)
  expect(matches(item, `${adjective}-${noun}`)).toBe(true)
  expect(matches(item, `machine ${noun}`)).toBe(true)
  expect(matches(item, 'task-')).toBe(false)
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
    ['unlisted project', 1],
    ['No project', 1],
  ])
  expect(projectTabs(items.slice(0, 1), projects).map((t) => t.key)).toEqual(['all', 'p-hdr'])
  expect(inProject(items[4], 'none')).toBe(true)
  expect(inProject(items[0], 'none')).toBe(false)
  expect(inProject(items[0], 'all')).toBe(true)
  expect(projectName(items[3], projects)).toBe('unlisted project')
  expect(projectName(items[4], projects)).toBeNull()
})

test('a project with nothing open gets a tab only while it is the one open', () => {
  const items = [
    view({ id: 'a1', project_id: 'p-hdr' }),
    view({ id: 'a2', project_id: 'p-old', readiness: { state: 'closed' }, state: 'closed' }),
    view({ id: 'a3', readiness: { state: 'closed' }, state: 'closed' }),
  ]
  const projects = [project('p-hdr', 'hounddogreading'), project('p-old', 'imported workspace')]
  expect(projectTabs(items, projects).map((t) => t.key)).toEqual(['all', 'p-hdr'])
  expect(projectTabs(items, projects, 'p-old').map((t) => t.key)).toEqual(['all', 'p-hdr', 'p-old'])
  expect(projectTabs(items, projects, 'none').map((t) => t.key)).toEqual(['all', 'p-hdr', 'none'])
})

test('pickers offer repositories and projects with work, and tell same-named ones apart', () => {
  const projects: Project[] = [
    { ...project('p-hdr', 'hounddogreading'), remote_url: 'github.com/h/h' },
    { ...project('ws-aaaaaa1', 'imported workspace'), total: 1 },
    project('ws-bbbbbb2', 'imported workspace'),
    project('ws-cccccc3', 'imported workspace'),
  ]
  expect(pickableProjects(projects).map((p) => p.id)).toEqual(['p-hdr', 'ws-aaaaaa1'])
  expect(pickableProjects(projects, 'ws-cccccc3').map((p) => p.id)).toEqual(['p-hdr', 'ws-aaaaaa1', 'ws-cccccc3'])
  expect(projectLabel(projects[0], projects)).toBe('hounddogreading')
  expect(projectLabel(projects[1], projects)).toBe('imported workspace #1')
  expect(projectLabel(projects[3], projects)).toBe('imported workspace #3')
})

test('published work reads as waiting on its merge, and on acceptance of the candidate that merged', () => {
  const plain = view({ session_id: 's1' })
  expect(workState(plain)).toBe('insession')
  expect(landing(plain)).toBe(null)

  const published = view({ session_id: 's1', change_url: 'https://forge/pr/7', change_candidate: 'c1', change_state: 'published' })
  expect(workState(published)).toBe('published')
  expect(workLabel(published)).toBe('Published')
  expect(landingLine(published)).toBe('published · awaiting merge')

  const gated = { ...published, acceptance: 'operator' as const }
  expect(landingLine(gated)).toBe('published · awaiting merge, then your acceptance')
  expect(landingLine({ ...gated, accepted_candidate: 'c1' })).toBe('published · accepted, awaiting merge')

  const merged = { ...gated, change_state: 'merged' as const }
  expect(landing(merged)).toBe('acceptance')
  expect(workLabel(merged)).toBe('Merged')
  expect(landingLine(merged)).toBe('merged · awaiting your acceptance')
  // Accepted for that candidate, it is done; a new candidate inherits no verdict.
  expect(landing({ ...merged, accepted_candidate: 'c1' })).toBe(null)
  expect(landing({ ...merged, accepted_candidate: 'c0' })).toBe('acceptance')

  // Closed on the forge without merging, it is back to its session.
  const dropped = { ...published, change_state: 'closed' as const }
  expect(landing(dropped)).toBe('abandoned')
  expect(workState(dropped)).toBe('insession')
  // A closed item is closed, whatever its change says.
  expect(workState(view({ ...merged, state: 'closed', readiness: { state: 'closed' } }))).toBe('closed')
})
