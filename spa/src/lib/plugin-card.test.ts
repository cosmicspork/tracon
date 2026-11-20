import { describe, expect, test } from 'bun:test'
import { askedBy, pluginCard } from './plugin-card'

describe('pluginCard', () => {
  test('reads a plugin card and none other', () => {
    expect(pluginCard('fake__query', { plugin: 'x' })).toBeNull()
    const card = pluginCard('plugin_card', {
      plugin: 'hoja',
      version: '0.2.0',
      title: 'Edit a record',
      summary: 'Why',
      content: [
        { format: 'word_diff', before: 'old words', after: 'new words' },
        { format: 'table', rows: [{ key: 'Kind', value: 'risk' }] },
        { format: 'html', text: '<b>' },
      ],
      requested_for: { kind: 'agent', session: 's1', harness: 'claude' },
    })!
    expect(card.plugin).toBe('hoja')
    expect(card.content[0]).toEqual({ format: 'word_diff', title: undefined, before: 'old words', after: 'new words' })
    expect(card.content[1]).toEqual({ format: 'table', title: undefined, rows: [{ key: 'Kind', value: 'risk' }] })
    // Unknown to the page: shown as its source, not dropped.
    expect(card.content[2].format).toBe('source')
    expect(askedBy(card)).toBe('plugin hoja 0.2.0, for claude')
  })

  test('a card raised for no one names only the plugin', () => {
    const card = pluginCard('plugin_card', { plugin: 'p', content: [], requested_for: null })!
    expect(askedBy(card)).toBe('plugin p')
    expect(
      askedBy(pluginCard('plugin_card', { plugin: 'p', requested_for: { harness: 'external', session: null } })!),
    ).toBe('plugin p, for external (external)')
  })
})
