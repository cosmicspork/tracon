import { expect, test } from 'bun:test'
import { cardRequest, editableFields, editedArguments, isEgress, isWarmUpEgress, stepUpOptions, withheldOptions } from './permission'

const card = (args: unknown, kind: string | null = 'tool') => ({
  kind,
  raw_input: JSON.stringify({ tool: 'issue_comment', arguments: args }),
})

test('the prose arguments of a brokered call are editable, identifiers are not', () => {
  const body = 'A comment long enough that it is prose, not an identifier.'
  expect(editableFields(card({ key: 'WRK-1', body }))).toEqual([{ key: 'body', value: body }])
  expect(editableFields(card({ key: 'WRK-1', body: 'one\ntwo' })).map((f) => f.key)).toEqual(['body'])
})

test("a harness's own request is never editable", () => {
  expect(editableFields(card({ body: 'x'.repeat(80) }, 'execute'))).toEqual([])
  expect(editableFields({ kind: 'tool', raw_input: 'not json' })).toEqual([])
})

const approvalCard = (tool: string, args: unknown) => ({
  kind: 'tool',
  raw_input: JSON.stringify({ tool, arguments: args, approval_id: 'a1' }),
})

test('an approval card offers no field to edit: its page edits it, against what it changes', () => {
  const body = '# Plan: foo\n\nA body long enough to be prose on any other card.\n'
  expect(editableFields(approvalCard('doc_write', { slug: 'plan-foo', body }))).toEqual([])
  expect(editableFields(approvalCard('pr_comment', { repo: 'o/r', number: 1, body: 'x'.repeat(80) }))).toEqual([])
})

test('an approval card shows no request; a peer’s, which cannot open its page, shows all of it', () => {
  const body = '# Plan: foo\n\nOne line changed.\n'
  const write = approvalCard('doc_write', { slug: 'plan-foo', body, if_hash: 'abc' })
  expect(cardRequest(write, false)).toBeNull()
  expect(JSON.parse(cardRequest(write, true)!).arguments).toEqual({ slug: 'plan-foo', body, if_hash: 'abc' })
  expect(editableFields(write, true)).toEqual([{ key: 'body', value: body }])
  // A harness's own permission keeps its full request.
  expect(cardRequest(card({ body }), false)).toContain('One line changed')
  expect(cardRequest({ kind: 'tool', raw_input: 'not json' }, false)).toBe('not json')
})

test('only a change is sent, merged over the original arguments', () => {
  const c = card({ key: 'WRK-1', body: 'x'.repeat(50) })
  expect(editedArguments(c, {})).toBeUndefined()
  expect(editedArguments(c, { body: 'x'.repeat(50) })).toBeUndefined()
  expect(editedArguments(c, { body: 'rewritten' })).toEqual({ key: 'WRK-1', body: 'rewritten' })
})

test('an egress ask is told apart from a held call', () => {
  const ask = { raw_input: JSON.stringify({ tool: 'request_egress', arguments: { host: 'pypi.org' }, approval_id: 'a1' }) }
  expect(isEgress(ask)).toBe(true)
  expect(isEgress(card({ body: 'x' }))).toBe(false)
  expect(isEgress({ raw_input: null })).toBe(false)
  const warmUp = { raw_input: JSON.stringify({ tool: 'prepare_egress', arguments: { host: 'files.example', repo: '/r' }, approval_id: 'a2' }) }
  expect(isEgress(warmUp)).toBe(true)
  expect(isWarmUpEgress(warmUp)).toBe(true)
  expect(isWarmUpEgress(ask)).toBe(false)
})

test('a peer leaves the answers that need the owning node’s passkey to that node', () => {
  const held = (tool: string) => ({ raw_input: JSON.stringify({ tool, arguments: {}, approval_id: 'a1' }) })
  expect(stepUpOptions(held('repo_setup_propose'))).toEqual(['allow_once'])
  expect(stepUpOptions(held('repo_setup_try'))).toEqual(['allow_once'])
  expect(stepUpOptions(held('request_egress'))).toEqual(['allow_repo'])
  expect(stepUpOptions(held('prepare_egress'))).toEqual(['allow_repo'])
  expect(stepUpOptions(held('pr_comment'))).toEqual([])
  expect(stepUpOptions({ raw_input: JSON.stringify({ tool: 'repo_setup_try' }) })).toEqual([])
  expect(stepUpOptions({ raw_input: 'not json' })).toEqual([])
})

test('a card is only this node’s own when its owner is known to be this node', () => {
  const setup = (node_id: string) => ({
    node_id,
    raw_input: JSON.stringify({ tool: 'repo_setup_propose', arguments: {}, approval_id: 'a1' }),
  })
  const nodes = [
    { id: 'self', is_self: true },
    { id: 'peer', is_self: false },
  ] as Parameters<typeof withheldOptions>[0]
  expect(withheldOptions(nodes, setup('self'))).toEqual([])
  expect(withheldOptions(nodes, setup('peer'))).toEqual(['allow_once'])
  // Before the node list has loaded, or for an owner it does not name.
  expect(withheldOptions([], setup('self'))).toEqual(['allow_once'])
  expect(withheldOptions(nodes, setup('gone'))).toEqual(['allow_once'])
})
