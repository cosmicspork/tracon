import { expect, test } from 'bun:test'
import { defaultChannel } from './channel'

const names = ['personal', 'work']

test('this browser\'s last choice comes first, if the channel is still open', () => {
  expect(defaultChannel({ names, remembered: 'work', nodeDefault: 'personal' })).toBe('work')
  expect(defaultChannel({ names, remembered: 'nu', nodeDefault: 'personal' })).toBe('personal')
})

test('then the node\'s preference, then what ran last, then the first name', () => {
  const sessions = [
    { channel: 'personal', created_ms: 10 },
    { channel: 'work', created_ms: 20 },
    { channel: 'nu', created_ms: 30 },
  ]
  expect(defaultChannel({ names, nodeDefault: 'work' })).toBe('work')
  expect(defaultChannel({ names, nodeDefault: '' , sessions })).toBe('work')
  expect(defaultChannel({ names })).toBe('personal')
  expect(defaultChannel({ names: [] })).toBe('')
})
