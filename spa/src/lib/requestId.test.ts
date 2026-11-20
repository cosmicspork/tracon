import { describe, expect, test } from 'bun:test'
import { newRequestId, StartRequest } from './requestId'

describe('newRequestId', () => {
  test('is a v4 UUID, which the node takes as a request id', () => {
    const id = newRequestId()
    expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
    expect(newRequestId()).not.toBe(id)
  })
})

describe('StartRequest', () => {
  const counting = () => {
    let n = 0
    return new StartRequest(() => `req-${++n}`)
  }

  test('a start retried after an unknown outcome is sent with the same id', () => {
    const r = counting()
    const spec = { channel: 'personal', work_item_id: 'w1', node_id: 'peer' }
    expect(r.idFor(spec)).toBe('req-1')
    r.settle('outcome_unknown')
    expect(r.idFor({ ...spec })).toBe('req-1')
  })

  test('a settled start, or a changed form, gets a new id', () => {
    const r = counting()
    const spec = { channel: 'personal', work_item_id: 'w1' }
    expect(r.idFor(spec)).toBe('req-1')
    r.settle(undefined)
    expect(r.idFor(spec)).toBe('req-2')
    r.settle('outcome_unknown')
    expect(r.idFor({ ...spec, branch: 'feat/x' })).toBe('req-3')
    r.settle('unreachable')
    expect(r.idFor({ ...spec, branch: 'feat/x' })).toBe('req-4')
  })
})
