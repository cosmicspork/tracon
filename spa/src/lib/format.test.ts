import { expect, test } from 'bun:test'
import type { CodexLimits } from './types'
import { codexLimitLabel, digits, externalAgent, formatAge, formatCodexLimits, formatWindow, formatBudget, formatDuration, formatExpiry, formatGrouped, formatTokens } from './format'

test('tokens are shown with a k or M suffix once they earn one', () => {
  expect(formatTokens(512)).toBe('512')
  expect(formatTokens(15024)).toBe('15k')
  expect(formatTokens(1_240_000)).toBe('1.24M')
  expect(formatTokens(2_000_000)).toBe('2M')
})

test('a budget reads as used over total', () => {
  expect(formatBudget(412_000, 2_000_000)).toBe('412k/2M')
})

test('a duration is the largest unit that fits, and only that unit', () => {
  const s = 1000
  const m = 60 * s
  const h = 60 * m
  const d = 24 * h
  expect(formatDuration(48 * s)).toBe('48s')
  expect(formatDuration(12 * m)).toBe('12m')
  // The minutes remainder is deliberately gone: 2h10 asked to be read twice.
  expect(formatDuration(130 * m)).toBe('2h')
  expect(formatDuration(2 * h)).toBe('2h')
  expect(formatDuration(3 * d)).toBe('3d')
  expect(formatDuration(2 * 7 * d)).toBe('2w')
  expect(formatDuration(90 * d)).toBe('3mo')
  expect(formatDuration(400 * d)).toBe('1y')
})

test('each unit hands over at its own boundary', () => {
  const s = 1000
  const m = 60 * s
  const h = 60 * m
  const d = 24 * h
  expect(formatDuration(59 * s)).toBe('59s')
  expect(formatDuration(60 * s)).toBe('1m')
  expect(formatDuration(59 * m)).toBe('59m')
  expect(formatDuration(60 * m)).toBe('1h')
  expect(formatDuration(23 * h)).toBe('23h')
  expect(formatDuration(24 * h)).toBe('1d')
  expect(formatDuration(6 * d)).toBe('6d')
  expect(formatDuration(7 * d)).toBe('1w')
  expect(formatDuration(34 * d)).toBe('4w')
  expect(formatDuration(35 * d)).toBe('1mo')
  expect(formatDuration(364 * d)).toBe('12mo')
  expect(formatDuration(365 * d)).toBe('1y')
})

test('an age is a duration measured back from now, and never negative', () => {
  const now = 1_000_000_000
  expect(formatAge(now - 48_000, now)).toBe('48s')
  expect(formatAge(now - 130 * 60_000, now)).toBe('2h')
  // 139 hours: the shape that read as a session running for six days.
  expect(formatAge(now - 139 * 3_600_000, now)).toBe('5d')
  expect(formatAge(now + 5_000, now)).toBe('0s')
})

test('an expiry counts down and then says so', () => {
  const now = 1_000_000_000
  expect(formatExpiry(now + 240_000, now)).toBe('expires 4m')
  expect(formatExpiry(now + 30_000, now)).toBe('expires 30s')
  expect(formatExpiry(now - 1, now)).toBe('expired')
})

test('a typed number is shown grouped and read back as digits', () => {
  expect(formatGrouped(2_000_000)).toBe('2,000,000')
  expect(formatGrouped(999)).toBe('999')
  expect(formatGrouped(0)).toBe('0')
  expect(digits('2,000,000')).toBe(2_000_000)
  expect(digits('1 500k')).toBe(1500)
  expect(digits('')).toBe(0)
})

test('an external agent is named by the tail of its id', () => {
  expect(externalAgent('01a0e964-9d4a-7872-b04c-f7d19aa8a4b8')).toBe('agent 9aa8a4b8')
  expect(externalAgent(null)).toBe('')
  expect(externalAgent(undefined)).toBe('')
  expect(externalAgent('01a0e964-9d4a-7872-b04c-f7d19aa8a4b8', 'tracon:main#4242')).toBe('tracon:main#4242')
  expect(externalAgent(null, '')).toBe('')
})

test('a subscription window reads as how much of it is used and when it resets', () => {
  const now = 1_800_000_000_000
  const lines = formatCodexLimits(
    {
      channel: 'work',
      provider: 'openai-codex',
      limit_id: 'codex',
      observed_ms: now - 3 * 60_000,
      usage_id: 1,
      primary: { used_percent: 42.4, window_minutes: 300, resets_at: now / 1000 + 2 * 3600 },
      secondary: { used_percent: 7, window_minutes: 10_080, resets_at: null },
      credits: { has_credits: true, unlimited: false, balance: '12.50' },
      limit_name: null,
      reached_type: null,
    },
    now,
  )
  expect(lines).toEqual(['42% of 5h used · resets in 2h', '7% of 7d used', 'credits 12.50', 'observed 3m ago'])
  expect(formatWindow(90)).toBe('90m')
})

function limits(now: number, over: Partial<CodexLimits>): CodexLimits {
  return {
    channel: 'work',
    provider: 'openai-codex',
    limit_id: 'codex',
    observed_ms: now,
    usage_id: null,
    primary: null,
    secondary: null,
    credits: null,
    limit_name: null,
    reached_type: null,
    ...over,
  }
}

test('a refused subscription says why while its window has yet to reset', () => {
  const now = 1_800_000_000_000
  const lines = formatCodexLimits(
    limits(now, {
      primary: { used_percent: 100, window_minutes: null, resets_at: now / 1000 + 600 },
      credits: { has_credits: false, unlimited: null, balance: null },
      reached_type: 'rate_limit_reached',
    }),
    now,
  )
  expect(lines).toEqual(['limit reached (rate_limit_reached)', '100% used · resets in 10m', 'no credits', 'observed 0s ago'])
})

test('a refusal whose windows have all reset since reads as reset, not as reached', () => {
  // A 429 at 10:00 said 95% of 5h, resetting at 12:00; it is now 16:00.
  const at = 1_800_000_000_000
  const now = at + 6 * 3600_000
  const lines = formatCodexLimits(
    limits(at, {
      primary: { used_percent: 95, window_minutes: 300, resets_at: at / 1000 + 2 * 3600 },
      reached_type: 'rate_limit_reached',
    }),
    now,
  )
  expect(lines).toEqual(['5h window reset since (was 95%)', 'observed 6h ago'])
  expect(lines.some((l) => l.startsWith('limit reached'))).toBe(false)
  expect(lines.some((l) => /\d+%( of \S+)? used/.test(l))).toBe(false)
})

test('a refusal that was not about a window outlasts the window resetting', () => {
  // A 429 at 10:00 said the credits ran out, with 20% of 5h resetting at 12:00; it is now 16:00.
  const at = 1_800_000_000_000
  const now = at + 6 * 3600_000
  const lines = formatCodexLimits(
    limits(at, {
      primary: { used_percent: 20, window_minutes: 300, resets_at: at / 1000 + 2 * 3600 },
      reached_type: 'workspace_owner_credits_depleted',
    }),
    now,
  )
  expect(lines).toEqual(['limit reached (workspace_owner_credits_depleted)', '5h window reset since (was 20%)', 'observed 6h ago'])
})

test('a refusal stays shown while any reported window has yet to reset', () => {
  const now = 1_800_000_000_000
  const lines = formatCodexLimits(
    limits(now, {
      primary: { used_percent: 30, window_minutes: 300, resets_at: now / 1000 - 60 },
      secondary: { used_percent: 100, window_minutes: 10_080, resets_at: now / 1000 + 86_400 },
      reached_type: 'rate_limit_reached',
    }),
    now,
  )
  expect(lines.slice(0, 3)).toEqual(['limit reached (rate_limit_reached)', '5h window reset since (was 30%)', '100% of 7d used · resets in 1d'])
})

test('a snapshot of another limit is named, so it does not pass for the plan', () => {
  const now = 1_800_000_000_000
  expect(codexLimitLabel(limits(now, {}))).toBe('openai-codex')
  expect(codexLimitLabel(limits(now, { limit_id: 'codex_other', limit_name: 'gpt-5-codex' }))).toBe('openai-codex · gpt-5-codex')
  expect(codexLimitLabel(limits(now, { limit_id: 'codex_other' }))).toBe('openai-codex · codex_other')
})
