import { expect, test } from 'bun:test'
import { attention, judged } from './criteria'

test('machine checks do not count as human judgement', () => {
  expect(judged('checks_pass')).toBe(false)
  expect(judged('judged_met')).toBe(true)
  expect(judged('judged_not_met')).toBe(true)
  expect(judged('judged_unclear')).toBe(true)
})

test('what needs attention is a gap or a failure, never a green', () => {
  expect(attention('nothing_points_at_it')).toBe('gap')
  expect(attention('only_proposed')).toBe('gap')
  expect(attention('failing')).toBe('fail')
  expect(attention('judged_not_met')).toBe('fail')
  expect(attention('judged_met')).toBe('settled')
  // Checks passing is waiting on a person, not done.
  expect(attention('checks_pass')).toBe('wait')
  expect(attention('awaits_judgement')).toBe('wait')
})
