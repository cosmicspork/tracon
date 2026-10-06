import { describe, expect, test } from 'bun:test'
import { laneKey, liveness, prune, visibleLanes } from './externalLanes'
import type { ExternalLane } from './types'

const lane = (over: Partial<ExternalLane> = {}): ExternalLane => ({
  channel: 'work',
  lane: 'repo:main',
  last_ms: 100,
  calls: 3,
  pending: 0,
  running: null,
  ...over,
})

describe('external lanes', () => {
  test('a dismissed lane stays hidden until it calls again', () => {
    const a = lane()
    const dismissed = { [laneKey(a)]: 100 }
    expect(visibleLanes([a], dismissed)).toEqual([])
    expect(visibleLanes([lane({ last_ms: 101 })], dismissed)).toHaveLength(1)
  })

  test('an unlabelled lane and a labelled one on the same channel are different lanes', () => {
    expect(laneKey(lane({ lane: null }))).not.toBe(laneKey(lane()))
    expect(laneKey(lane({ channel: 'personal' }))).not.toBe(laneKey(lane()))
  })

  test('dismissals for lanes no longer listed are dropped', () => {
    const kept = lane()
    const gone = lane({ lane: 'repo:old' })
    const pruned = prune({ [laneKey(kept)]: 1, [laneKey(gone)]: 1 }, [kept])
    expect(Object.keys(pruned)).toEqual([laneKey(kept)])
  })

  test('liveness says unknown when no process was named', () => {
    expect(liveness(lane())).toBe('unknown')
    expect(liveness(lane({ running: 2 }))).toBe('running')
    expect(liveness(lane({ running: 0 }))).toBe('gone')
  })
})
