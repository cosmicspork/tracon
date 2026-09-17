import { expect, test } from 'bun:test'
import {
  groupLog,
  missingContext,
  missingLine,
  orientationLine,
  groupOpen,
  groupSummary,
  providerErrorLine,
  repetitionHint,
  repetitionLine,
  usageMismatchLine,
  usageUnmeteredLine,
} from './log'
import type { Event } from './types'

let seq = 0
function ev(kind: string, ref_id: string | null = null, payload: Record<string, unknown> = {}): Event {
  seq += 1
  return { seq, node_id: 'n', session_id: 's', kind, ref_id, payload, at_ms: 0, mono_ms: seq }
}

test('consecutive tool calls fold into one group', () => {
  const log = groupLog([
    ev('user_prompt'),
    ev('tool_call', 'a', { kind: 'read' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'read' }),
    ev('tool_result', 'b', { status: 'completed' }),
    ev('message'),
  ])
  expect(log.map((l) => l.kind)).toEqual(['leaf', 'tools', 'leaf'])
  expect(log[1].tools!.length).toBe(2)
  expect(groupOpen(log[1].tools!)).toBe(false)
})

test('a group stays open while a call lacks its result', () => {
  const log = groupLog([ev('tool_call', 'a', { kind: 'execute' })])
  expect(groupOpen(log[0].tools!)).toBe(true)
})

test('a permission request breaks the group at its true position', () => {
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'execute' }),
    ev('permission_request', 'p1'),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'execute' }),
  ])
  expect(log.map((l) => l.kind)).toEqual(['tools', 'leaf', 'tools'])
  // The result still lands on its call even across the break.
  expect(log[0].tools![0].result).toBeDefined()
})

test('summary counts by kind and reports failures', () => {
  const log = groupLog([
    ev('tool_call', 'a', { kind: 'read' }),
    ev('tool_result', 'a', { status: 'completed' }),
    ev('tool_call', 'b', { kind: 'read' }),
    ev('tool_result', 'b', { status: 'completed' }),
    ev('tool_call', 'c', { kind: 'execute' }),
    ev('tool_result', 'c', { status: 'failed' }),
  ])
  expect(groupSummary(log[0].tools!)).toBe('Read 2 files, ran 1 shell command · 1 failed')
})

test('an orphan tool_result is kept as a leaf', () => {
  const log = groupLog([ev('tool_result', 'ghost', { status: 'completed' })])
  expect(log[0].kind).toBe('leaf')
})

test('a provider error reads as a retry in progress', () => {
  expect(providerErrorLine({ provider: 'anthropic', status: 429, message: 'Error', attempt: 2 })).toBe(
    'anthropic answered 429 · Error · harness retrying · attempt 2',
  )
  // The first attempt needs no number, and a harness notice may carry neither
  // a status nor a message.
  expect(providerErrorLine({ provider: 'anthropic', status: 429, attempt: 1 })).toBe(
    'anthropic answered 429 · harness retrying',
  )
  expect(providerErrorLine({})).toBe('the provider refused the call · harness retrying')
})

test('repetition reads as a signal to look at, not as a verdict', () => {
  expect(repetitionLine({ count: 3, title: 'run just test' })).toBe(
    'same call 3× in a row · run just test · recorded, not paused',
  )
  expect(repetitionLine({})).toBe('same call 0× in a row · the same tool call · recorded, not paused')
})

test('the repetition hint stands only until the harness moves on', () => {
  const signal = ev('repetition', null, { count: 3, title: 'run just test' })
  expect(repetitionHint([ev('tool_call'), signal])).toBe(
    'same call 3× in a row · run just test · recorded, not paused',
  )
  // A turn ending, a pause or a resume all end the run it was describing.
  expect(repetitionHint([signal, ev('turn_end')])).toBeNull()
  expect(repetitionHint([signal, ev('session_paused')])).toBeNull()
  expect(repetitionHint([])).toBeNull()
})

test('a usage disagreement shows both numbers and what was charged', () => {
  expect(
    usageMismatchLine({
      gateway: { tokens: 10000, requests: 4 },
      harness: { tokens: 12 },
      charged_tokens: 10000,
    }),
  ).toBe('usage disagrees · gateway 10k · harness 12 · charged 10k')
  // A harness that said nothing is not a harness that said zero.
  expect(usageMismatchLine({ gateway: { tokens: 50 }, harness: { tokens: null }, charged_tokens: 50 })).toBe(
    'usage disagrees · gateway 50 · harness reported nothing · charged 50',
  )
})

test('an unmetered turn is named as unknown, never as zero', () => {
  const line = usageUnmeteredLine({ gateway: { tokens: 0, requests: 3 } })
  expect(line).toContain('3 model calls')
  expect(line).toContain('unknown rather than zero')
  expect(usageUnmeteredLine({ gateway: { requests: 1 } })).toContain('1 model call')
})

test('an orientation that lost context names how many pieces, not just that it was cut', () => {
  const payload = {
    chars: 22812,
    trimmed: true,
    missing: [
      { what: 'guide "Workspace" (`guide-workspace`)', partial: true, chars: 8000, fetch: 'call `doc_read` for `guide-workspace`' },
      { what: 'guide "Release" (`guide-release`)', partial: false, chars: 16000, fetch: 'call `doc_read` for `guide-release`' },
    ],
  }
  expect(orientationLine(payload)).toBe('orientation · 23k chars · 2 pieces not included in full')
  expect(orientationLine({ chars: 900 })).toBe('orientation · 900 chars')
  expect(orientationLine({ chars: 1, missing: [{ what: 'the diff', partial: true, chars: 40 }] })).toContain(
    '1 piece not included in full',
  )

  // Each omission names the document and how to fetch it: an operator cannot
  // act on "something was trimmed".
  const [cut, absent] = missingContext(payload)
  expect(missingLine(cut)).toBe(
    'guide "Workspace" (`guide-workspace`) cut short, 8k chars — call `doc_read` for `guide-workspace`',
  )
  expect(missingLine(absent)).toContain('not included, 16k chars')
  expect(missingLine({ what: '3 more directives and facts', partial: false, chars: 8000 })).toBe(
    '3 more directives and facts not included, 8k chars',
  )
})

test('an orientation event from an older node carries no omissions', () => {
  expect(missingContext({ chars: 100, trimmed: false })).toEqual([])
  expect(missingContext({ missing: 'not a list' })).toEqual([])
  expect(missingContext({ missing: [null, 7, { what: 'the plan', partial: true, chars: 5 }] })).toHaveLength(1)
  expect(orientationLine({ chars: 100, trimmed: true })).toBe('orientation · 100 chars · trimmed')
  expect(orientationLine({ chars: 100, trimmed: false })).toBe('orientation · 100 chars')
})
