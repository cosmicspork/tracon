import { expect, test } from 'bun:test'
import { groupDocs, kindLabel, kindOf } from './docs'
import type { DocKindInfo, Document } from './types'

const info = (kind: string, reserved = false): DocKindInfo => ({
  kind,
  label: kind[0].toUpperCase() + kind.slice(1),
  description: '',
  working_note: false,
  reserved,
})

// As the node lists its defaults: configured, reserved, then other.
const node = [
  ...['guide', 'ref', 'architecture', 'proposal', 'repo', 'note', 'meeting', 'inbox'].map((k) => info(k)),
  ...['brief', 'context', 'plan', 'shown', 'other'].map((k) => info(k, true)),
]

const doc = (slug: string, kind: string, o: Partial<Document> = {}): Document => ({
  id: slug,
  channel: 'personal',
  slug,
  kind,
  title: slug,
  body: '',
  hash: '',
  format: 'markdown',
  site: 'n1',
  hlc_ms: 0,
  deleted: 0,
  created_ms: 0,
  updated_ms: 0,
  ...o,
})

const kinds = (docs: Document[], list = node) => groupDocs(docs, list).map(([kind, group]) => `${kind}:${group.length}`)

test('a work item\'s selected context is listed under its own kind', () => {
  const docs = [doc('guide-a', 'guide'), doc('context-0192f3a17c4e', 'context'), doc('plan-b', 'plan')]
  expect(kinds(docs)).toEqual(['guide:1', 'context:1', 'plan:1'])
})

test('a kind the interface does not name still gets a group, before other', () => {
  const docs = [doc('scratch', 'other'), doc('runbook-x', 'runbook'), doc('note-y', 'note')]
  expect(kinds(docs)).toEqual(['note:1', 'runbook:1', 'other:1'])
})

test('every document is in exactly one group, archived ones last', () => {
  const docs = [
    doc('context-a', 'context'),
    doc('context-b', 'context', { archived: 1 }),
    doc('note-c', 'note'),
    doc('shown-d', 'shown'),
  ]
  const groups = groupDocs(docs, node)
  expect(groups.flatMap(([, list]) => list).length).toBe(docs.length)
  expect(groups.map(([kind]) => kind)).toEqual(['note', 'context', 'shown', 'archived'])
})

test('the groups follow the order the node lists its kinds in', () => {
  const docs = [doc('note-a', 'note'), doc('runbook-b', 'runbook')]
  const list = [info('runbook'), info('note'), info('other', true)]
  expect(kinds(docs, list)).toEqual(['runbook:1', 'note:1'])
})

test('a slug classifies by the node\'s kinds, and a kind it lost reads as other', () => {
  const list = [info('runbook'), info('plan', true), info('other', true)]
  expect(kindOf('runbook-deploy', list)).toBe('runbook')
  expect(kindOf('runbook', list)).toBe('runbook')
  expect(kindOf('runbooks-x', list)).toBe('other')
  expect(kindOf('meeting-weekly', list)).toBe('other')
  expect(kindLabel('runbook', list)).toBe('Runbook')
  expect(kindLabel('mystery', list)).toBe('mystery')
})
