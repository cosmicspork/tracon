import { describe, expect, test } from 'bun:test'
import { OPENER_MISSING, localOutgoing, type LocalInputs } from './outgoing'

const base: LocalInputs = {
  title: 'fix: the sync',
  body: 'The agent text.',
  forge: {},
  change: false,
  opensNone: false,
  generatedDetails: false,
  opener: '',
}

describe('localOutgoing', () => {
  test('a new change opens with the approved title and body', () => {
    expect(localOutgoing(base)).toEqual({
      description: { title: 'fix: the sync', body: 'The agent text.' },
      comment: null,
      refusal: null,
    })
  })

  test('a description of its own replaces the summary, and the comment goes with it', () => {
    const out = localOutgoing({ ...base, forge: { description: { title: 't', body: 'b' }, comment: 'c' } })
    expect(out).toEqual({ description: { title: 't', body: 'b' }, comment: 'c', refusal: null })
  })

  test('a push-only update sends no description', () => {
    expect(localOutgoing({ ...base, change: true, forge: { comment: 'c' } }).description).toBeNull()
  })

  test('a first push or a direct push sends nothing', () => {
    expect(localOutgoing({ ...base, opensNone: true, forge: { comment: 'c' } })).toEqual({
      description: null,
      comment: null,
      refusal: null,
    })
  })

  test('under generated details the opener leads, and without one approval is refused', () => {
    expect(localOutgoing({ ...base, generatedDetails: true })).toEqual({
      description: null,
      comment: null,
      refusal: OPENER_MISSING,
    })
    expect(localOutgoing({ ...base, generatedDetails: true, opener: 'Why.' }).description?.body).toBe(
      'Why.\n\n<details>\n<summary>Generated details</summary>\n\nThe agent text.\n\n</details>',
    )
  })
})
