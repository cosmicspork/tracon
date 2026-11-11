import { describe, expect, test } from 'bun:test'
import { capabilityName, capabilityTone, elsewhereLines } from './capabilities'
import type { Elsewhere } from './types'

const line = (value: string | null) => ({ capability: 'route', value, check: 'network_isolated', detail: '' })

describe('capabilities', () => {
  test('names capabilities as words', () => {
    expect(capabilityName('runtime_socket')).toBe('runtime socket')
    expect(capabilityName('image_identity')).toBe('image identity')
    expect(capabilityName('tier')).toBe('tier')
  })

  test('marks what was not established and what is a caution', () => {
    expect(capabilityTone(line(null))).toBe('off')
    expect(capabilityTone(line('unverified'))).toBe('warn')
    expect(capabilityTone(line('provider-open'))).toBe('warn')
    expect(capabilityTone(line('present'))).toBe('warn')
    expect(capabilityTone(line('node-only'))).toBe('')
  })
})

const there = (over: Partial<Elsewhere> = {}): Elsewhere => ({
  reach: 'hub url',
  dial: 'wss://hub.example.net/v0/relay/connect',
  egress: ['hub.example.net'],
  placement: 'overflow',
  healthy: true,
  capacity: { running: 1, limit: 4 },
  services: [],
  ...over,
})

describe('a provider elsewhere', () => {
  test('says whether it can take a session, and why not', () => {
    expect(elsewhereLines(there())[0]).toEqual({ label: 'health', tone: '', text: 'can take a session' })
    const down = elsewhereLines(there({ healthy: false, why: 'its template is being built' }))[0]
    expect(down.tone).toBe('bad')
    expect(down.detail).toBe('its template is being built')
  })

  test('names its way in, marking the hub URL a caution', () => {
    const [, reach] = elsewhereLines(there())
    expect(reach.text).toBe('hub url · wss://hub.example.net/v0/relay/connect')
    expect(reach.tone).toBe('warn')
    expect(reach.detail).toContain('egress admits hub.example.net')
    const [, direct] = elsewhereLines(there({ reach: 'address', dial: '203.0.113.7:443', egress: ['203.0.113.7'] }))
    expect(direct.tone).toBe('')
    expect(direct.detail).toContain("the node's own address")
  })

  test('counts what runs there against its limit', () => {
    expect(elsewhereLines(there())[2].text).toBe('overflow · 1 of 4 running')
    expect(elsewhereLines(there({ capacity: { running: 0, limit: null } }))[2].text).toBe('overflow · 0 running')
  })

  test('lists each service and why one is refused', () => {
    const lines = elsewhereLines(
      there({
        services: [
          { name: 'browser', ok: true },
          { name: 'pg', ok: false, why: 'give it a bundle' },
        ],
      }),
    )
    expect(lines.slice(3)).toEqual([
      { label: 'service browser', tone: '', text: 'runs inside' },
      { label: 'service pg', tone: 'off', text: 'refused', detail: 'give it a bundle' },
    ])
  })
})
