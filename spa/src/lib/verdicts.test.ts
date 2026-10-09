import { describe, expect, test } from 'bun:test'
import { composerLabel, publishedChange, unavailable } from './verdicts'

const ready = { busy: false, publishing: false, stale: [] as string[], reason: '' }

describe('verdict availability', () => {
  test('approve needs nothing but a current, settled review', () => {
    expect(unavailable('approve', ready)).toBeNull()
    expect(unavailable('approve', { ...ready, stale: ['a.txt'] })).toContain('a.txt changed since submit')
    expect(unavailable('approve', { ...ready, stale: ['a', 'b'] })).toContain('2 files')
    expect(unavailable('approve', { ...ready, publishing: true })).toContain('reconcile')
  })

  test('approve says why when this node cannot publish it', () => {
    expect(unavailable('approve', { ...ready, readiness: { ready: true } })).toBeNull()
    const problem = 'no GitHub token (GITHUB_TOKEN) is stored on this node'
    expect(unavailable('approve', { ...ready, readiness: { ready: false, problem } })).toBe(
      `cannot publish from this node yet: ${problem}`,
    )
    expect(unavailable('approve', { ...ready, readiness: { ready: false } })).toContain('cannot publish')
    // Sending it back needs no token.
    expect(unavailable('revise', { ...ready, reason: 'x', readiness: { ready: false } })).toBeNull()
    // A stale review is named first: binding a token would not make it approvable.
    expect(unavailable('approve', { ...ready, stale: ['a'], readiness: { ready: false } })).toContain('changed since submit')
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

  test('the published change is read from the forge URL', () => {
    expect(publishedChange('https://github.com/o/r/pull/418')).toEqual({ url: 'https://github.com/o/r/pull/418', number: 418 })
    expect(publishedChange('https://gitlab.example/g/p/-/merge_requests/7#note_1')?.number).toBe(7)
    expect(publishedChange('https://forge.example/o/r/compare/main...x')).toEqual({ url: 'https://forge.example/o/r/compare/main...x', number: null })
    expect(publishedChange('pushed abc123')).toBeNull()
    expect(publishedChange(null)).toBeNull()
    expect(publishedChange('')).toBeNull()
  })
})
