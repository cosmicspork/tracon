import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'

import {
  afterAllow,
  feed,
  notRunDetail,
  notRunLine,
  OUTCOME_WAIT_MS,
  settledFrom,
  STALE_OUTCOME,
  ToastGate,
  TOAST_RECENT_MS,
  type AllowedOutcome,
} from './allowed-outcome'

const NOW = 1_000_000_000

const staleReason = 'stale: plugin sql-review: the rows it changes moved since it was proposed'

function settled(state: string, reason: string | null = null, id = 'approval-d'): AllowedOutcome {
  const o = settledFrom('approval_settled', id, { approval_id: id, tool: 'sql-review__write', state, reason }, NOW, 's1')
  if (!o) throw new Error('not an outcome')
  return o
}

describe('an approval_settled event', () => {
  test('is read as how the allowed call ended, stale when the plugin said so', () => {
    const o = settled('failed', staleReason)
    expect(o).toMatchObject({ id: 'approval-d', tool: 'sql-review__write', state: 'failed', stale: true, session_id: 's1' })
    expect(settled('failed', 'not mergeable').stale).toBe(false)
    expect(settled('uncertain').stale).toBe(false)
  })

  test('any other event, or a state that is not an end, is not an outcome', () => {
    expect(settledFrom('permission_answer', 'a', { state: 'running' }, NOW)).toBeNull()
    expect(settledFrom('approval_settled', 'a', { state: 'expired' }, NOW)).toBeNull()
    expect(settledFrom('approval_settled', null, { state: 'failed' }, NOW)).toBeNull()
  })
})

describe('the approval page after an accepted Allow', () => {
  test('keeps following while the call runs, up to its wait, then leaves it to the toast', () => {
    expect(afterAllow(undefined, 0).kind).toBe('running')
    expect(afterAllow(undefined, OUTCOME_WAIT_MS - 1).kind).toBe('running')
    expect(afterAllow(undefined, OUTCOME_WAIT_MS).kind).toBe('leave')
  })

  test('shows Ran for a call that succeeded, then leaves', () => {
    expect(afterAllow(settled('succeeded'), 100).kind).toBe('ran')
  })

  test('stays and shows the stale banner when the allowed approval ends failed with stale', () => {
    const next = afterAllow(settled('failed', staleReason), 100)
    expect(next.kind).toBe('stay')
    expect(notRunDetail('failed', staleReason)).toBe(STALE_OUTCOME)
    expect(STALE_OUTCOME).toBe('The rows changed since you looked; nothing was sent. The agent has to ask again.')
    // Past the wait, an outcome that arrived still keeps the page.
    expect(afterAllow(settled('uncertain'), OUTCOME_WAIT_MS * 2).kind).toBe('stay')
  })

  test('says what happened for any run that did not go through, and nothing for one that did', () => {
    expect(notRunDetail('failed', 'not mergeable')).toContain('did not run')
    expect(notRunDetail('uncertain', null)).toContain('could not be confirmed')
    expect(notRunDetail('succeeded', null)).toBeNull()
  })
})

describe('the shell toast', () => {
  test('is raised for a failed allowed call when its page is not open', () => {
    const gate = new ToastGate()
    const toast = gate.notice(settled('failed', staleReason), '/', new Set(), NOW)
    expect(toast).toEqual({
      id: 'approval-d',
      text: "sql-review__write didn't run: the rows changed since you looked",
      href: '/approvals/approval-d',
    })
    // Once only, however often the stream or a path change shows it again.
    expect(gate.notice(settled('failed', staleReason), '/sessions', new Set(), NOW)).toBeNull()
  })

  test('is raised for an uncertain one, and not for one that ran', () => {
    const gate = new ToastGate()
    expect(gate.notice(settled('uncertain', null, 'u'), '/', new Set(), NOW)?.text).toContain('may not have run')
    expect(gate.notice(settled('succeeded', null, 'ok'), '/', new Set(), NOW)).toBeNull()
  })

  test('is not raised while the approval page is open, or for one already opened, or an old one replayed', () => {
    expect(new ToastGate().notice(settled('failed'), '/approvals/approval-d', new Set(), NOW)).toBeNull()
    expect(new ToastGate().notice(settled('failed'), '/', new Set(['approval-d']), NOW)).toBeNull()
    expect(new ToastGate().notice(settled('failed'), '/', new Set(), NOW + TOAST_RECENT_MS + 1)).toBeNull()
  })
})

describe('the home feed of calls that did not run', () => {
  test('merges the queue and the stream once each, newest first, until opened', () => {
    const queued: AllowedOutcome[] = [
      { id: 'a', tool: 't', title: 'Card A', state: 'failed', reason: 'boom', stale: false, at_ms: 10 },
      { id: 'b', tool: 't', title: 'Card B', state: 'uncertain', reason: null, stale: false, at_ms: 20 },
    ]
    const streamed = [settled('failed', staleReason, 'a'), settled('succeeded', null, 'c')]
    const rows = feed(queued, streamed, new Set())
    expect(rows.map((o) => o.id)).toEqual(['a', 'b'])
    // The stream's outcome, with the queue's title.
    expect(notRunLine(rows[0]!)).toBe("Card A didn't run: the rows changed since you looked")
    expect(feed(queued, streamed, new Set(['a'])).map((o) => o.id)).toEqual(['b'])
  })

  test('a failure names its reason without the stale marker', () => {
    expect(notRunLine({ id: 'x', tool: 'pr_merge', state: 'failed', reason: 'not mergeable', stale: false, at_ms: 0 })).toBe(
      "pr_merge didn't run: not mergeable",
    )
  })
})

// The page's markup and script, read as source: the wiring is a race, and
// the order of a branch is what decides it.
describe('the approval page source', () => {
  const page = readFileSync(new URL('../routes/Approval.svelte', import.meta.url), 'utf8')
  const answer = page.slice(page.indexOf('async function answer('), page.indexOf('</script>'))

  test('follows an accepted Allow instead of leaving at once', () => {
    const allow = answer.indexOf("if (optionId === 'allow_once'")
    expect(allow).toBeGreaterThan(-1)
    expect(answer.indexOf('await follow()')).toBeGreaterThan(allow)
    expect(answer.indexOf('await follow()')).toBeLessThan(answer.indexOf('leaveAfterVerdict('))
  })

  test('a stale 409 reloads with the fresh preview and says so, ahead of any other 409', () => {
    const stale = answer.indexOf("e.status === 409 && e.kind === 'stale'")
    const other = answer.indexOf('e.status === 409)')
    expect(stale).toBeGreaterThan(-1)
    expect(stale).toBeLessThan(other)
    expect(page).toContain('{STALE_ALLOW}')
  })

  test('the outcome banner says what happened to a call that did not run', () => {
    const banner = page.indexOf('{outcome(a.state, a.reason)}')
    expect(page.indexOf('notRunDetail(a.state, a.reason)', banner)).toBeGreaterThan(banner)
  })

  test('the shell mounts the toasts', () => {
    const app = readFileSync(new URL('../App.svelte', import.meta.url), 'utf8')
    expect(app).toContain('<OutcomeToasts />')
  })
})
