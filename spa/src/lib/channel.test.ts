import { expect, test } from 'bun:test'
import { defaultChannel, providerChannelSeed } from './channel'

const names = ['personal', 'work']

test('the node\'s preference comes first, if the channel is still open', () => {
  expect(defaultChannel({ names, remembered: 'personal', nodeDefault: 'work' })).toBe('work')
  expect(defaultChannel({ names, remembered: 'personal', nodeDefault: 'nu' })).toBe('personal')
})

test('then this browser\'s last choice, then what ran last, then the first name', () => {
  const sessions = [
    { channel: 'personal', created_ms: 10 },
    { channel: 'work', created_ms: 20 },
    { channel: 'nu', created_ms: 30 },
  ]
  expect(defaultChannel({ names, remembered: 'work', nodeDefault: '' })).toBe('work')
  expect(defaultChannel({ names, nodeDefault: '' , sessions })).toBe('work')
  expect(defaultChannel({ names })).toBe('personal')
  expect(defaultChannel({ names: [] })).toBe('')
})

test('a sign-in serves the node\'s preferred channel unless it already served others', () => {
  const channels = [
    { name: 'personal', nodes: ['n1'] },
    { name: 'work', nodes: ['n1', 'n2'] },
    { name: 'old', nodes: ['n1'], archived: 1 },
    { name: 'theirs', nodes: ['n2'] },
  ]
  expect(providerChannelSeed({ channels, nodeId: 'n1', nodeDefault: 'work' })).toEqual(['work'])
  expect(providerChannelSeed({ channels, nodeId: 'n1', nodeDefault: 'theirs' })).toEqual(['personal', 'work'])
  expect(providerChannelSeed({ channels, nodeId: 'n1' })).toEqual(['personal', 'work'])
  expect(providerChannelSeed({ channels, nodeId: 'n1', nodeDefault: 'work', existing: ['personal', 'old'] })).toEqual([
    'personal',
  ])
  // A node no channel lists yet is offered every open channel.
  expect(providerChannelSeed({ channels, nodeId: 'n9' })).toEqual(['personal', 'work', 'theirs'])
})
