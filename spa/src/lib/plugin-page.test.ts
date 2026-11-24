import { expect, test } from 'bun:test'
import { otherChannels, pageChannel, pluginDefaultChannel, pluginHref, pluginTabTitle } from './plugin-page'
import type { PluginSummary } from './types'

const plugin = (offered_on: string[] | null, o: Partial<PluginSummary> = {}): PluginSummary => ({
  name: 'workboard',
  repo: 'cosmicspork/workboard',
  state: 'active',
  active: { version: '0.4.0', tools: [], ui: true, ui_title: 'Workboard' },
  instances: [],
  offered_on,
  ...o,
})

test("the node's default channel, when the plugin is offered there", () => {
  expect(pluginDefaultChannel(['personal', 'work'], 'work')).toBe('work')
  expect(pageChannel('', plugin(['personal', 'work']), 'work')).toEqual({ channel: 'work' })
})

test("otherwise the first channel it is offered on, when the node's default is not one", () => {
  expect(pluginDefaultChannel(['work', 'ops'], 'personal')).toBe('work')
  expect(pluginDefaultChannel(['work', 'ops'], null)).toBe('work')
  expect(pageChannel('', plugin(['work', 'ops']), 'personal')).toEqual({ channel: 'work' })
})

test('a single offered channel, and none', () => {
  expect(pluginDefaultChannel(['work'], 'personal')).toBe('work')
  expect(otherChannels(['work'], 'personal')).toEqual([])
  expect(pluginDefaultChannel([], 'personal')).toBeNull()
  expect(pluginDefaultChannel(null, 'personal')).toBeNull()
  expect(pageChannel('', plugin([]), 'personal')).toEqual({ error: 'is not offered on any channel' })
})

test('the per-channel buttons are every offered channel but the default', () => {
  expect(otherChannels(['personal', 'work', 'ops'], 'work')).toEqual(['personal', 'ops'])
  expect(otherChannels(['personal', 'work', 'ops'], 'nope')).toEqual(['work', 'ops'])
})

test('the address names the channel; one the plugin is not offered on is an error, not a fall back', () => {
  expect(pageChannel('?channel=ops', plugin(['work', 'ops']), 'work')).toEqual({ channel: 'ops' })
  expect(pageChannel('?channel=personal', plugin(['work', 'ops']), 'work')).toEqual({
    error: 'is not offered on channel personal',
  })
  expect(pageChannel('', undefined, 'work')).toEqual({ error: 'not installed on this node' })
  expect(pageChannel('', plugin(['work'], { active: { version: '1', tools: [], ui: false } }), 'work')).toEqual({
    error: 'has no page',
  })
})

test('addresses and the tab title', () => {
  expect(pluginHref('work board')).toBe('/plugins/work%20board')
  expect(pluginHref('workboard', 'a b')).toBe('/plugins/workboard?channel=a%20b')
  expect(pluginTabTitle('Workboard', 'work')).toBe('Workboard · work')
})
