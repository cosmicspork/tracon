import { expect, test } from 'bun:test'
import { groupDocs } from './docs'
import type { Document } from './types'

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

const kinds = (docs: Document[]) => groupDocs(docs).map(([kind, list]) => `${kind}:${list.length}`)

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
  const groups = groupDocs(docs)
  expect(groups.flatMap(([, list]) => list).length).toBe(docs.length)
  expect(groups.map(([kind]) => kind)).toEqual(['context', 'note', 'shown', 'archived'])
})
