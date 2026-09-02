import { expect, test } from 'bun:test'
import { humanizeError } from './errors'

test('nothing to say about no error', () => {
  expect(humanizeError(null)).toBe(null)
  expect(humanizeError(undefined)).toBe(null)
  expect(humanizeError('   ')).toBe(null)
})

test('the rpc wrappers come off', () => {
  expect(humanizeError('rpc: rpc error -32603: Internal error (Unknown tool tracon__ask)')).toBe(
    'the harness called a tool this node does not offer',
  )
  expect(humanizeError('rpc: rpc error -32603: Internal error')).toBe('the harness hit an internal error')
  expect(humanizeError('rpc: rpc error -32601: Method not found: session/new')).toBe(
    'the harness asked for something this node does not implement',
  )
})

test("the node's own sentences are kept, in the operator's voice", () => {
  expect(humanizeError('node restarted while the session was live')).toBe(
    'the node restarted while this session was live',
  )
  expect(humanizeError('lost on owner')).toBe('the node that owned this session lost it')
})

test('anything else is passed through, trimmed', () => {
  expect(humanizeError('  model gpt-9 is not offered by this node  ')).toBe(
    'model gpt-9 is not offered by this node',
  )
  expect(humanizeError('rpc: rpc error -32000: the gateway refused api.example.com')).toBe(
    'the gateway refused api.example.com',
  )
})
