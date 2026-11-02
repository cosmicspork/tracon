import { describe, expect, test } from 'bun:test'
import { capabilityName, capabilityTone } from './capabilities'

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
