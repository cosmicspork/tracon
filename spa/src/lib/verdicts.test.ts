import { describe, expect, test } from 'bun:test'
import { composerLabel, unavailable } from './verdicts'

const ready = { busy: false, publishing: false, stale: [] as string[], reason: '' }

describe('verdict availability', () => {
  test('approve needs nothing but a current, settled review', () => {
    expect(unavailable('approve', ready)).toBeNull()
    expect(unavailable('approve', { ...ready, stale: ['a.txt'] })).toContain('a.txt changed since submit')
    expect(unavailable('approve', { ...ready, stale: ['a', 'b'] })).toContain('2 files')
    expect(unavailable('approve', { ...ready, publishing: true })).toContain('reconcile')
  })

  test('a verdict that sends a reason says it needs one', () => {
    expect(unavailable('revise', ready)).toBe('say what should change')
    expect(unavailable('reject', { ...ready, reason: '   ' })).toBe('say why you are rejecting it')
    expect(unavailable('revise', { ...ready, reason: 'rename it' })).toBeNull()
    // A stale review can still be sent back: that is how it gets resubmitted.
    expect(unavailable('revise', { ...ready, reason: 'x', stale: ['a'] })).toBeNull()
  })

  test('the composer is labelled for what it sends', () => {
    expect(composerLabel('reject', 0)).toContain('rejecting')
    expect(composerLabel('revise', 2)).toContain('2 files')
    expect(composerLabel('revise', 0)).toBe('What should change? The agent reads this.')
  })
})
