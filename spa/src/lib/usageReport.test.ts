import { expect, test } from 'bun:test'
import { usageView } from './routes'
import type { InfraTotal, UsageTotal } from './types'
import {
  formatHours,
  infraCsvHref,
  infraKey,
  infraParams,
  infraSpendLine,
  usageCost,
  usageCsvHref,
  usageHeading,
  usageKey,
  usageParams,
  usageProviders,
  usageSpan,
} from './usageReport'

const infra = (over: Partial<InfraTotal>): InfraTotal => ({
  provider: 'digitalocean',
  channel: null,
  day: null,
  session_id: null,
  size: null,
  kind: null,
  sandboxes: 1,
  hours: 1,
  cost_usd: null,
  unpriced: 0,
  ...over,
})

test('the infrastructure section asks for the same window, grouped its own way', () => {
  const view = usageView('?since=2026-10-01&until=2026-10-15&provider=openai-codex&group=model')
  const q = infraParams(view, 'size')
  expect(Number(q.get('since_ms'))).toBe(new Date('2026-10-01T00:00:00').getTime())
  expect(Number(q.get('until_ms'))).toBe(new Date('2026-10-16T00:00:00').getTime())
  expect(q.get('group')).toBe('size')
  // Sandboxes have no model provider to filter by.
  expect(q.get('provider')).toBeNull()
  expect(infraCsvHref(view, 'size')).toBe(`/api/usage/infra.csv?${q}`)
})

test('an infrastructure row is keyed by its day, channel, size or session', () => {
  expect(infraKey(infra({ day: '2026-10-02' }), 'day')).toBe('2026-10-02')
  expect(infraKey(infra({ channel: 'work' }), 'channel')).toBe('work')
  expect(infraKey(infra({}), 'channel')).toBe('no channel')
  expect(infraKey(infra({ size: 'small' }), 'size')).toBe('small')
  expect(infraKey(infra({ session_id: 's1' }), 'session')).toBe('s1')
  expect(infraKey(infra({ kind: 'check' }), 'session')).toBe('check run')
  expect(formatHours(0.75)).toBe('45m')
  expect(formatHours(3.21)).toBe('3.2h')
  const spend = { today_usd: 1.2, limit_usd: 5, state: 'under' as const, running: 1, unpriced: 0 }
  expect(infraSpendLine(spend)).toBe('$1.20 of $5.00 today')
  expect(infraSpendLine({ ...spend, limit_usd: null, state: 'none' })).toBe('$1.20 today, no limit')
})

const row = (over: Partial<UsageTotal>): UsageTotal => ({
  channel: 'work',
  provider: 'openai-codex',
  model: 'gpt-5',
  requests: 1,
  input_tokens: 100,
  output_tokens: 10,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
  sessions: 1,
  cost_usd: null,
  ...over,
})

test('the breakdown asks for the window, provider and grouping its address names', () => {
  const view = usageView('?since=2026-10-01&until=2026-10-15&provider=openai-codex&group=day')
  const q = usageParams(view)
  expect(Number(q.get('since_ms'))).toBe(new Date('2026-10-01T00:00:00').getTime())
  expect(Number(q.get('until_ms'))).toBe(new Date('2026-10-16T00:00:00').getTime())
  expect(q.get('provider')).toBe('openai-codex')
  expect(q.get('group')).toBe('day')
  expect(usageCsvHref(view)).toBe(`/api/usage.csv?${q}`)
  // The default asks for no end, no provider and no grouping.
  expect([...usageParams(usageView(''), 1e12).keys()]).toEqual(['since_ms', 'tz_offset_minutes'])
})

test('the breakdown sends the browser zone at the start, in minutes east of UTC', () => {
  const tz = process.env.TZ
  const offset = (search: string, zone: string) => {
    process.env.TZ = zone
    return usageParams(usageView(search)).get('tz_offset_minutes')
  }
  try {
    // CDT in October, CST in December: the start's own offset, not today's.
    expect(offset('?since=2026-10-01&until=2026-10-15', 'America/Chicago')).toBe('-300')
    expect(offset('?since=2026-12-01&until=2026-12-15', 'America/Chicago')).toBe('-360')
    expect(offset('?since=2026-10-01&until=2026-10-15', 'Asia/Kolkata')).toBe('330')
  } finally {
    if (tz === undefined) delete process.env.TZ
    else process.env.TZ = tz
  }
})

test('a breakdown row is keyed by its model, its day or its session', () => {
  expect(usageKey(row({}), usageView(''))).toBe('gpt-5')
  expect(usageKey(row({ model: null }), usageView(''))).toBe('unknown model')
  expect(usageKey(row({ day: '2026-10-02' }), usageView('?group=day'))).toBe('2026-10-02')
  const bySession = usageView('?group=session')
  expect(usageKey(row({ session_id: 's1', title: 'Ship it', review: 'accepted' }), bySession)).toBe('Ship it · accepted')
  expect(usageKey(row({ session_id: 's1' }), bySession)).toBe('s1')
  expect(usageKey(row({}), bySession)).toBe('outside a session')
  expect(usageHeading(bySession)).toBe('By provider and session')
})

test('the filter offers each provider once and the table prices where it can', () => {
  const rows = [row({ provider: 'openai-codex' }), row({ provider: 'anthropic' }), row({ provider: 'openai-codex' })]
  expect(usageProviders(['openai'], rows, null)).toEqual(['anthropic', 'openai', 'openai-codex'])
  // A provider filtered to nothing is still there to clear.
  expect(usageProviders([], [], 'openai-codex')).toEqual(['openai-codex'])
  expect(usageCost(null)).toBe('unpriced')
  expect(usageCost(0.125)).toBe('$0.13')
  expect(usageSpan(usageView('?since=2026-10-01&until=2026-10-15'))).toBe('2026-10-01 – 2026-10-15')
})
