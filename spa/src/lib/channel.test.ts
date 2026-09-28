import { expect, test } from 'bun:test'
import { defaultChannel } from './channel'

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
