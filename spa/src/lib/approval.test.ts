import { expect, test } from 'bun:test'
import {
  approvalArguments,
  approvalFields,
  CALLER_ONLY,
  changedArguments,
  editProblems,
  grantsAccess,
  outcome,
  problemsByField,
  amendsNothing,
  replacement,
  resultRows,
  schemaFor,
} from './approval'
import { formValues, type JsonSchema } from './schema-form'

const schema: JsonSchema = {
  type: 'object',
  required: ['slug', 'text'],
  properties: {
    slug: { type: 'string' },
    if_hash: { type: 'string' },
    text: { type: 'string' },
    title: { type: 'string' },
    labels: { type: 'array', items: { type: 'string' } },
    count: { type: 'integer' },
  },
}
const args = { slug: 'plan-x', if_hash: 'abc', text: 'one line', title: 'A title' }

test("the node's prose and locked fields win over schema-form's guesses", () => {
  const fields = approvalFields(schema, args, ['text'], ['slug', 'if_hash'])
  const by = Object.fromEntries(fields.map((f) => [f.key, f]))
  expect(by.text!.kind).toBe('prose')
  expect(by.title!.kind).toBe('text')
  expect(by.slug!.locked).toBe(true)
  expect(by.if_hash!.locked).toBe(true)
  expect(by.text!.locked).toBe(false)

  const keyed = approvalFields({ properties: { key: { type: 'string' }, description: { type: 'string' } } }, { key: 'X-1', description: 'short' }, [], [])
  expect(keyed.map((f) => [f.key, f.kind, f.locked])).toEqual([
    ['key', 'text', false],
    ['description', 'text', false],
  ])
})

test('a multi-line string is prose even when the node does not name it', () => {
  const [f] = approvalFields({ properties: { note: { type: 'string' } } }, { note: 'a\nb' }, [], [])
  expect(f!.kind).toBe('prose')
})

test('an untouched form is no edit, and a locked field always goes back as it came', () => {
  const fields = approvalFields(schema, args, ['text'], ['slug', 'if_hash'])
  const values = formValues(fields, args)
  const same = approvalArguments(fields, values, args)
  expect(changedArguments(args, same)).toBeUndefined()

  const tampered = approvalArguments(fields, { ...values, slug: 'other', text: 'rewritten' }, args)
  expect(tampered.slug).toBe('plan-x')
  expect(changedArguments(args, tampered)).toEqual({ ...args, text: 'rewritten' })
})

test('only what the operator changed is held against the schema', () => {
  const original = { slug: 'plan-x', text: 'ok', count: 'agent sent a string' }
  expect(editProblems(schema, original, original)).toEqual([])
  expect(editProblems(schema, original, { ...original, labels: ['a', 2] as unknown as string[] })).toEqual([
    { path: 'labels[1]', message: 'must be a string' },
  ])
  expect(editProblems(schema, original, { slug: 'plan-x', count: 'agent sent a string' })).toEqual([
    { path: 'text', message: 'is required' },
  ])
})

test("the node's 422 fields land under the input they belong to", () => {
  const got = problemsByField(
    [
      { field: 'labels[1]', message: 'must be a string' },
      { field: 'title', message: 'is too long' },
      { field: 'notes', message: 'say what should change' },
      { field: '', message: 'the arguments must be an object' },
      { path: 'nowhere', message: 'is unknown' },
    ],
    ['labels', 'title', 'notes'],
  )
  expect(got).toEqual({
    labels: ['labels[1]: must be a string'],
    title: ['is too long'],
    notes: ['say what should change'],
    '': ['the arguments must be an object', 'nowhere: is unknown'],
  })
})

test('without a schema the form is read off the arguments', () => {
  const s = schemaFor(null, { body: 'x', n: 2, tags: ['a'], nested: { a: 1 } })
  expect(s.properties).toEqual({
    body: { type: 'string' },
    n: { type: 'integer' },
    tags: { type: 'array', items: { type: 'string' } },
    nested: {},
  })
  expect(schemaFor(schema, {})).toBe(schema)
})

test('a settled approval says what became of it', () => {
  expect(outcome('pending')).toBeNull()
  expect(outcome('changes_requested')).toBe('Changes requested · nothing ran')
  expect(outcome('something_new')).toBe('something_new')
})

test('locked fields lead the form, in schema order', () => {
  const fields = approvalFields(schema, args, ['text'], ['slug', 'if_hash'])
  expect(fields.map((f) => f.key)).toEqual(['slug', 'if_hash', 'text', 'title', 'labels', 'count'])
})

test('a list edited one entry per line keeps commas inside an entry', () => {
  const s: JsonSchema = { properties: { checks: { type: 'array', items: { type: 'string' } } } }
  const original = { checks: ['a'] }
  const fields = approvalFields(s, original, [], [])
  const typed = 'rg -n "a, b" src\n\n  cargo test  \n'.split('\n')
  expect(approvalArguments(fields, { checks: typed }, original)).toEqual({ checks: ['rg -n "a, b" src', 'cargo test'] })
})

test('an answer that opens a scope grants access instead of running an edit', () => {
  expect(grantsAccess([{ kind: 'allow_once' }, { kind: 'reject_once' }])).toBe(false)
  expect(grantsAccess([{ kind: 'allow_once' }, { kind: 'allow_session' }])).toBe(true)
  expect(CALLER_ONLY).toContain('wait_secs')
})

test('a result reads as rows, with links where they are URLs', () => {
  expect(resultRows({ url: 'https://x.test/1', id: 7, gone: null })).toEqual([
    { key: 'url', value: 'https://x.test/1', href: 'https://x.test/1' },
    { key: 'id', value: '7', href: null },
  ])
  expect(resultRows('done')).toBeNull()
  expect(resultRows([1])).toBeNull()
})

test('a document write diffs the draft against the document as it stands, or as all added when new', () => {
  const document = { body: '# Plan\n\nOne.\nTwo.', hash: 'h', format: 'markdown', title: 'Plan', updated_ms: 1 }
  const edit = replacement({ document }, { slug: 'plan-foo', body: '# Plan\n\nOne.\nThree.' })!
  expect(edit).toMatchObject({ created: false, title: 'Plan', format: 'markdown', added: 1, removed: 1 })
  // The operator's edit is the draft the diff is taken from.
  expect(replacement({ document }, { body: document.body })).toMatchObject({ added: 0, removed: 0 })
  const fresh = replacement({ document: null }, { body: '# New\n\nBody.' })!
  expect(fresh).toMatchObject({ created: true, title: null, added: 3, removed: 0 })
  // A tool that replaces nothing sends no document at all.
  expect(replacement({}, { body: 'x' })).toBeNull()
})

test('a document edit diffs the body its edits make, sent by the node, against the document', () => {
  const document = { body: '# Plan\n\nOne.\nTwo.', hash: 'h', format: 'markdown', title: 'Plan', updated_ms: 1 }
  const edits = [{ old_str: 'Two.', new_str: 'Three.' }]
  const edit = replacement({ document, proposed_body: '# Plan\n\nOne.\nThree.' }, { slug: 'plan-foo', edits })!
  expect(edit).toMatchObject({ created: false, title: 'Plan', added: 1, removed: 1 })
  // Edits that no longer apply make no body: nothing is shown as changed.
  expect(replacement({ document, proposed_body: null }, { slug: 'plan-foo', edits })).toMatchObject({
    added: 0,
    removed: 0,
  })
  // A stray `body` argument can't stand in for the edits' body on the card.
  expect(
    replacement({ document, proposed_body: '# Plan\n\nOne.\nThree.' }, { slug: 'plan-foo', edits, body: document.body }),
  ).toMatchObject({ added: 1, removed: 1 })
})

test('the diff names a change of format, from the document to what the call writes', () => {
  const html = { body: '<p>x</p>', hash: 'h', format: 'html', title: 'Page', updated_ms: 1 }
  expect(replacement({ document: html }, { body: '# x' })!.format).toBe('html → markdown')
  expect(replacement({ document: html }, { body: '<p>y</p>', format: 'html' })!.format).toBe('html')
  expect(replacement({ document: null }, { body: '<p>y</p>', format: 'html' })!.format).toBe('html')
  expect(replacement({ document: null }, { body: '# y' })!.format).toBe('markdown')
})

test('a memory amendment diffs the draft against the memory as it stands, and says when it is gone', () => {
  const memory = {
    id: 'm1',
    kind: 'fact',
    scope: 'global',
    scope_ref: null,
    body: 'The importer retries twice.\nIt logs each retry.',
    state: 'active',
    updated_ms: 1,
  }
  const edit = replacement({ memory }, { id: 'm1', body: 'The importer retries three times.\nIt logs each retry.' })!
  expect(edit).toMatchObject({
    subject: 'memory',
    label: 'edit · fact · global',
    created: false,
    format: null,
    added: 1,
    removed: 1,
  })
  expect(edit.hunks).toEqual([
    {
      kind: 'lines',
      lines: [
        { kind: 'del', text: 'The importer retries twice.' },
        { kind: 'add', text: 'The importer retries three times.' },
        { kind: 'same', text: 'It logs each retry.' },
      ],
    },
  ])
  expect(amendsNothing({ memory })).toBe(false)
  // Gone while it waited: nothing to diff, and the page says Allow would fail.
  expect(replacement({ memory: null }, { body: 'x' })).toBeNull()
  expect(amendsNothing({ memory: null })).toBe(true)
  // A document write is headed as one, and nothing else is a memory.
  const document = { body: 'a', hash: 'h', format: 'markdown', title: 'Plan', updated_ms: 1 }
  expect(replacement({ document }, { body: 'b' })).toMatchObject({ subject: 'document', label: 'edit · “Plan”' })
  expect(replacement({ document: null }, { body: 'b' })).toMatchObject({ label: 'new document' })
  expect(amendsNothing({})).toBe(false)
})
