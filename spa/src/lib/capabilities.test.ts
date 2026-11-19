import { describe, expect, test } from 'bun:test'
import { capabilityName, capabilityTone, elsewhereLines, memoryWords, roomLine, roomWords } from './capabilities'
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
  expected_uid: 65532,
  reported: null,
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

  test('prices its size and holds today’s spend to the limit', () => {
    expect(elsewhereLines(there()).some((l) => l.label === 'default size' || l.label === 'spend')).toBe(false)
    const spend = { today_usd: 4.5, limit_usd: 5, state: 'near' as const, running: 2, unpriced: 0 }
    const lines = elsewhereLines(there({ size: 'small', price_per_hour_usd: 0.12, spend }))
    expect(lines.find((l) => l.label === 'default size')?.text).toBe('small · $0.120/h')
    const near = lines.find((l) => l.label === 'spend')
    expect(near?.text).toBe('$4.50 of $5.00 today')
    expect(near?.tone).toBe('warn')
    const at = elsewhereLines(there({ spend: { ...spend, today_usd: 5, state: 'at' } })).find((l) => l.label === 'spend')
    expect(at?.tone).toBe('bad')
    expect(at?.detail).toContain('limit reached')
    const none = elsewhereLines(there({ spend: { ...spend, limit_usd: null, state: 'none' } })).find((l) => l.label === 'spend')
    expect(none?.text).toBe('$4.50 today · no limit')
  })

  test('shows the default size with its memory and price', () => {
    const size = elsewhereLines(there({ size: 'mars-2vcpu-2gb', memory_mb: 2048, price_per_hour_usd: 0.107 })).find(
      (l) => l.label === 'default size',
    )
    expect(size).toMatchObject({ tone: '', text: 'mars-2vcpu-2gb · 2048 MB · $0.107/h' })
    expect(size?.detail).toContain('the cheapest with at least 2 GiB')
    const set = elsewhereLines(
      there({ size: 'mars-1vcpu-1gb', memory_mb: 1024, price_per_hour_usd: 0.0535, configured_size: 'mars-1vcpu-1gb' }),
    ).find((l) => l.label === 'default size')
    expect(set?.detail).toContain('set by [digitalocean] size')
  })

  test('shows each memory floor with the size it resolves to', () => {
    const lines = elsewhereLines(
      there({
        size: 'mars-2vcpu-2gb',
        memory_mb: 2048,
        price_per_hour_usd: 0.107,
        floors: [
          { scope: 'github.com/cosmicspork/tracon', memory: '4Gi', size: 'mars-2vcpu-4gb', memory_mb: 4096, price_per_hour_usd: 0.126 },
          {
            scope: 'channel big',
            memory: '64Gi',
            error: 'no DigitalOcean size has 64Gi; the largest is mars-16vcpu-32gb (32768 MB)',
          },
        ],
      }),
    )
    expect(lines.find((l) => l.label === 'floor github.com/cosmicspork/tracon')).toEqual({
      label: 'floor github.com/cosmicspork/tracon',
      tone: '',
      text: '4Gi → mars-2vcpu-4gb · 4096 MB · $0.126/h',
    })
    const none = lines.find((l) => l.label === 'floor channel big')
    expect(none).toMatchObject({ tone: 'bad', text: '64Gi → none' })
    expect(none?.detail).toContain('the largest is mars-16vcpu-32gb')
  })

  test('says why no size can be chosen: an unknown slug, or a catalog not read', () => {
    const unknown = elsewhereLines(
      there({
        configured_size: 'mars-1vcpu-512mb',
        size_error: "`[digitalocean] size` `mars-1vcpu-512mb` is not in DigitalOcean's catalog (mars-1vcpu-1gb)",
      }),
    ).find((l) => l.label === 'default size')
    expect(unknown).toMatchObject({ tone: 'bad', text: "mars-1vcpu-512mb · not in DigitalOcean's catalog" })
    expect(unknown?.detail).toContain('is not in')

    const unread = elsewhereLines(
      there({
        catalog_error: "could not read DigitalOcean's sandbox sizes: 503",
        size_error: "DigitalOcean's size catalog could not be read, so no size could be chosen for this sandbox",
      }),
    ).find((l) => l.label === 'default size')
    expect(unread).toMatchObject({ tone: 'bad', text: 'catalog not read' })
    expect(unread?.detail).toContain('503')

    // A configured size still starts sandboxes without the catalog, unpriced.
    const trusted = elsewhereLines(
      there({ size: 'mars-2vcpu-4gb', configured_size: 'mars-2vcpu-4gb', catalog_error: 'could not read: 503' }),
    ).find((l) => l.label === 'default size')
    expect(trusted).toMatchObject({ tone: 'warn', text: 'mars-2vcpu-4gb · unpriced' })
    expect(trusted?.detail).toContain('catalog not read')
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
    expect(lines.slice(4)).toEqual([
      { label: 'service browser', tone: '', text: 'runs inside' },
      { label: 'service pg', tone: 'off', text: 'refused', detail: 'give it a bundle' },
    ])
  })

  test('says who the harness runs as only once a sandbox has said', () => {
    expect(elsewhereLines(there())[3]).toMatchObject({ label: 'user', tone: 'off', text: 'not established' })
    const ok = elsewhereLines(there({ reported: { version: '0.50.0', uid: 65532 } }))[3]
    expect(ok).toMatchObject({ tone: '', text: 'uid 65532' })
    const root = elsewhereLines(there({ reported: { version: '0.48.1', uid: null } }))[3]
    expect(root.tone).toBe('bad')
    expect(root.detail).toContain('0.48.1 did not become uid 65532')
  })
})

describe("a cluster's room", () => {
  const full = { free_cpu: 763, free_memory: 660602880, needs_cpu: 1250, needs_memory: 1610612736, fits: false }
  test('says what it has free and what a session and its checks need', () => {
    expect(roomWords(full)).toBe('the cluster has 630 Mi free, a session and its checks need 1.5 Gi')
    expect(roomWords({ ...full, free_memory: 4 * 1024 ** 3, free_cpu: 500 })).toBe(
      'the cluster has 0.5 CPU free, a session and its checks need 1.25 CPU',
    )
    expect(roomWords({ ...full, free_memory: 4 * 1024 ** 3, free_cpu: 2000, fits: true })).toBe(
      'the cluster has 4 Gi and 2 CPU free; a session and its checks need 1.5 Gi and 1.25 CPU',
    )
    expect(roomWords({ running: 2, limit: 4 })).toBe('')
    expect(roomWords(null)).toBe('')
    expect(memoryWords(512 * 1024 * 1024)).toBe('512 Mi')
  })

  test('says why it was not read, and a pod the scheduler refused', () => {
    expect(roomWords({ unread: 'the node may not read node n1' })).toBe("the cluster's room was not read: the node may not read node n1")
    expect(roomWords({ fits: false, unschedulable: 'could not be scheduled: 0/1 nodes are available' })).toBe(
      'its pod could not be scheduled: 0/1 nodes are available',
    )
  })

  test('is a line beside the runtime capabilities', () => {
    expect(roomLine(full)).toEqual({
      label: 'room',
      tone: 'warn',
      text: 'full',
      detail: 'the cluster has 630 Mi free, a session and its checks need 1.5 Gi: a new session overflows or is refused',
    })
    expect(roomLine({ unread: 'forbidden' })?.tone).toBe('off')
    expect(roomLine(undefined)).toBeNull()
  })
})
