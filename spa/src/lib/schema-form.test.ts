import { describe, expect, test } from 'bun:test'
import { formArguments, formFields, formValues, validate, type JsonSchema } from './schema-form'

// The brokered prose tools' input schemas, as the node declares them.
const issueCreate: JsonSchema = {
  type: 'object',
  properties: {
    project: { type: 'string', description: 'Project key, e.g. WRK.' },
    type: { type: 'string', description: 'Issue type name, e.g. Task.' },
    summary: { type: 'string' },
    description: { type: 'string' },
    priority: { type: 'string' },
    labels: { type: 'array', items: { type: 'string' } },
    parent: { type: 'string', description: 'Parent issue key.' },
  },
  required: ['project', 'type', 'summary'],
}
const issueUpdate: JsonSchema = {
  type: 'object',
  properties: {
    key: { type: 'string' },
    summary: { type: 'string' },
    description: { type: 'string' },
    priority: { type: 'string' },
    labels: { type: 'array', items: { type: 'string' } },
    parent: { type: 'string' },
  },
  required: ['key'],
}
const issueComment: JsonSchema = {
  type: 'object',
  properties: { key: { type: 'string' }, body: { type: 'string' } },
  required: ['key', 'body'],
}
const prComment: JsonSchema = {
  type: 'object',
  properties: { repo: { type: 'string' }, number: { type: 'integer' }, body: { type: 'string' } },
  required: ['repo', 'number', 'body'],
}
const mrComment: JsonSchema = {
  type: 'object',
  properties: { project: { type: 'string' }, iid: { type: 'integer' }, body: { type: 'string' } },
  required: ['project', 'iid', 'body'],
}

describe('field model', () => {
  test('issue_create: short strings, prose, tags, and the locked target', () => {
    const fields = formFields(issueCreate)
    expect(fields.map((f) => [f.key, f.kind, f.required, f.locked])).toEqual([
      ['project', 'text', true, true],
      ['type', 'text', true, false],
      ['summary', 'text', true, false],
      ['description', 'prose', false, false],
      ['priority', 'text', false, false],
      ['labels', 'tags', false, false],
      ['parent', 'text', false, false],
    ])
    expect(fields[0]!.description).toBe('Project key, e.g. WRK.')
  })

  test('comment tools: the body is prose and the target is locked', () => {
    for (const [schema, target] of [
      [issueComment, ['key']],
      [prComment, ['repo', 'number']],
      [mrComment, ['project', 'iid']],
    ] as const) {
      const fields = formFields(schema)
      expect(fields.find((f) => f.key === 'body')!.kind).toBe('prose')
      expect(fields.filter((f) => f.locked).map((f) => f.key)).toEqual([...target])
    }
    expect(formFields(prComment).find((f) => f.key === 'number')!.kind).toBe('integer')
    expect(formFields(issueUpdate).find((f) => f.key === 'key')!.locked).toBe(true)
  })

  test('a multi-line value makes any string prose; an enum is a select', () => {
    expect(formFields(issueUpdate, { summary: 'a\nb' }).find((f) => f.key === 'summary')!.kind).toBe('prose')
    const fields = formFields({
      type: 'object',
      properties: {
        level: { type: 'string', enum: ['low', 'high'] },
        flag: { type: 'boolean' },
        extra: { type: 'object' },
        ids: { type: 'array', items: { type: 'integer' } },
      },
    })
    expect(fields.map((f) => [f.key, f.kind])).toEqual([
      ['level', 'select'],
      ['flag', 'boolean'],
      ['extra', 'json'],
      ['ids', 'json'],
    ])
    expect(fields[0]!.options).toEqual(['low', 'high'])
  })
})

describe('form round trip', () => {
  test('unchanged values give back the original arguments', () => {
    const args = { project: 'WRK', type: 'Task', summary: 'Do it', labels: ['a', 'b'], description: 'x\ny' }
    const fields = formFields(issueCreate, args)
    expect(formArguments(fields, formValues(fields, args), args)).toEqual(args)

    const pr = { repo: 'owner/name', number: 12, body: 'hi' }
    const prFields = formFields(prComment, pr)
    expect(formValues(prFields, pr).number).toBe('12')
    expect(formArguments(prFields, formValues(prFields, pr), pr)).toEqual(pr)
  })

  test('edits are typed, blanks drop optional fields, unknown arguments pass through', () => {
    const args = { project: 'WRK', type: 'Task', summary: 'Do it', priority: 'High', extra: 1 }
    const fields = formFields(issueCreate, args)
    const values = { ...formValues(fields, args), priority: '  ', labels: [' one ', '', 'two'], parent: 'WRK-1' }
    expect(formArguments(fields, values, args)).toEqual({
      project: 'WRK',
      type: 'Task',
      summary: 'Do it',
      labels: ['one', 'two'],
      parent: 'WRK-1',
      extra: 1,
    })
  })

  test('a value that does not parse is kept for the validator to report', () => {
    const pr = { repo: 'owner/name', number: 12, body: 'hi' }
    const fields = formFields(prComment, pr)
    const out = formArguments(fields, { ...formValues(fields, pr), number: '12a' }, pr)
    expect(out.number).toBe('12a')
    expect(validate(prComment, out)).toEqual([{ path: 'number', message: 'must be an integer' }])
  })
})

describe('validate', () => {
  test('valid arguments have no errors', () => {
    expect(validate(issueCreate, { project: 'WRK', type: 'Task', summary: 's', labels: ['a'] })).toEqual([])
    expect(validate(mrComment, { project: 'g/p', iid: 3, body: 'b' })).toEqual([])
  })

  test('missing required, wrong types, and bad array items are each reported by path', () => {
    expect(validate(issueCreate, { project: 'WRK', summary: 5, labels: ['a', 2] })).toEqual([
      { path: 'type', message: 'is required' },
      { path: 'summary', message: 'must be a string' },
      { path: 'labels[1]', message: 'must be a string' },
    ])
    expect(validate(mrComment, { project: 'g/p', iid: 1.5, body: 'b' })).toEqual([
      { path: 'iid', message: 'must be an integer' },
    ])
    expect(validate(issueComment, ['not', 'an', 'object'])).toEqual([{ path: '', message: 'must be an object' }])
  })

  test('enum and nested objects', () => {
    const schema: JsonSchema = {
      type: 'object',
      properties: {
        level: { enum: ['low', 'high'] },
        inner: { type: 'object', properties: { n: { type: ['integer', 'null'] } }, required: ['n'] },
      },
    }
    expect(validate(schema, { level: 'mid', inner: {} })).toEqual([
      { path: 'level', message: 'must be one of "low", "high"' },
      { path: 'inner.n', message: 'is required' },
    ])
    expect(validate(schema, { level: 'low', inner: { n: null } })).toEqual([])
  })
})
