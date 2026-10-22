import { expect, test } from 'bun:test'
import {
  exhaustionDefaults,
  exhaustionPatch,
  modelLabel,
  modelPatch,
  phaseDefaults,
  reviewTriggers,
  triggerPatch,
} from './bindings'

const bound = {
  phases: {
    plan: { model: 'openai-codex/gpt-5.6-sol', budget_tokens: 1_000_000 },
    execute: { model: 'openai-codex/gpt-5.6-terra' },
  },
}

test('a phase takes what its channel binds', () => {
  expect(phaseDefaults(bound, 'plan')).toEqual({
    model: 'openai-codex/gpt-5.6-sol',
    budget_tokens: 1_000_000,
  })
  expect(phaseDefaults(bound, 'execute')).toEqual({
    model: 'openai-codex/gpt-5.6-terra',
    budget_tokens: undefined,
  })
})

test('an unbound phase, channel, or blank value decides nothing', () => {
  expect(phaseDefaults(bound, 'review')).toEqual({})
  expect(phaseDefaults({}, 'plan')).toEqual({})
  expect(phaseDefaults(undefined, 'plan')).toEqual({})
  expect(phaseDefaults({ phases: { plan: { model: '  ' } } }, 'plan').model).toBe(undefined)
  expect(phaseDefaults({ phases: { plan: { budget_tokens: 0 } } }, 'plan').budget_tokens).toBe(undefined)
})

test('a model is shown by the name its node gave it', () => {
  const models = [{ name: 'GPT-5.6-Sol', value: 'openai-codex/gpt-5.6-sol' }]
  expect(modelLabel('openai-codex/gpt-5.6-sol', models)).toBe('GPT-5.6-Sol')
  expect(modelLabel('openai-codex/gpt-5.6-terra', models)).toBe('gpt-5.6-terra')
  expect(modelLabel(undefined, models)).toBe(null)
})

test('choosing nothing removes the binding', () => {
  expect(modelPatch('plan', 'm/a')).toEqual({ 'phases.plan.model': 'm/a' })
  expect(modelPatch('execute', '')).toEqual({ 'phases.execute.model': null })
  expect(modelPatch('execute', '   ')).toEqual({ 'phases.execute.model': null })
  expect(modelPatch('review', '')).toEqual({ 'phases.review.model': null })
})

test('a channel pauses on exhaustion unless it says otherwise', () => {
  expect(exhaustionDefaults({})).toEqual({ policy: 'pause', fallback: undefined })
  expect(exhaustionDefaults(undefined)).toEqual({ policy: 'pause', fallback: undefined })
  expect(exhaustionDefaults({ exhaustion: { policy: 'fallback', fallback: 'b/m' } })).toEqual({
    policy: 'fallback',
    fallback: 'b/m',
  })
  expect(exhaustionDefaults({ exhaustion: { policy: 'later' } } as never).policy).toBe('pause')
})

test('a fallback policy is only written with its model', () => {
  expect(exhaustionPatch('pause', 'b/m')).toEqual({ exhaustion: null })
  expect(exhaustionPatch('fallback', '')).toBe(null)
  expect(exhaustionPatch('fallback_then_wait', undefined)).toBe(null)
  expect(exhaustionPatch('fallback_then_wait', ' b/m ')).toEqual({
    exhaustion: { policy: 'fallback_then_wait', fallback: 'b/m' },
  })
})

/** The node's dotted-path merge (`merge_path`): null removes a key. */
function merged(bindings: Record<string, unknown>, patch: Record<string, unknown>): Record<string, unknown> {
  const out = structuredClone(bindings)
  for (const [path, value] of Object.entries(patch)) {
    const parts = path.split('.')
    let cur = out as Record<string, unknown>
    for (const part of parts.slice(0, -1)) {
      if (typeof cur[part] !== 'object' || cur[part] === null) cur[part] = {}
      cur = cur[part] as Record<string, unknown>
    }
    const last = parts.at(-1)!
    if (value === null) delete cur[last]
    else cur[last] = value
  }
  return out
}

test('a channel reviews its sessions on submission and external harnesses by hand unless it says otherwise', () => {
  expect(reviewTriggers({ bindings: {} })).toEqual({ session: 'auto', external: 'manual' })
  expect(reviewTriggers({ bindings: { phases: { review: { trigger: 'manual' } } } })).toEqual({
    session: 'manual',
    external: 'manual',
  })
  // What the node reports wins: its defaults are the ones that apply.
  expect(reviewTriggers({ bindings: {}, review_trigger: { session: 'auto', external: 'auto' } })).toEqual({
    session: 'auto',
    external: 'auto',
  })
})

test('a review trigger saved from settings reads back as saved', () => {
  let bindings: Record<string, unknown> = { phases: { review: { model: 'm/r' } } }
  bindings = merged(bindings, triggerPatch(bindings, 'external', 'auto'))
  expect(reviewTriggers({ bindings })).toEqual({ session: 'auto', external: 'auto' })
  bindings = merged(bindings, triggerPatch(bindings, 'session', 'manual'))
  expect(reviewTriggers({ bindings })).toEqual({ session: 'manual', external: 'auto' })
  // The model is left as it was, and the trigger saves without one.
  expect(phaseDefaults(bindings, 'review').model).toBe('m/r')
  const unbound = merged({}, triggerPatch({}, 'session', 'manual'))
  expect(reviewTriggers({ bindings: unbound })).toEqual({ session: 'manual', external: 'manual' })
})

test('one value set for both is split before one source changes', () => {
  const both = { phases: { review: { trigger: 'manual' } } }
  expect(triggerPatch(both, 'session', 'auto')).toEqual({
    'phases.review.trigger': { session: 'auto', external: 'manual' },
  })
  expect(reviewTriggers({ bindings: merged(both, triggerPatch(both, 'session', 'auto')) })).toEqual({
    session: 'auto',
    external: 'manual',
  })
})

test('a trigger value the node does not know is replaced whole, not written into', () => {
  const odd = { phases: { review: { trigger: 'sometimes' } } }
  // Read as the defaults, as the node reads it.
  expect(reviewTriggers({ bindings: odd })).toEqual({ session: 'auto', external: 'manual' })
  expect(triggerPatch(odd, 'external', 'auto')).toEqual({ 'phases.review.trigger': { external: 'auto' } })
  expect(reviewTriggers({ bindings: merged(odd, triggerPatch(odd, 'external', 'auto')) })).toEqual({
    session: 'auto',
    external: 'auto',
  })
  expect(triggerPatch({ phases: { review: { trigger: 3 } } }, 'session', 'manual')).toEqual({
    'phases.review.trigger': { session: 'manual' },
  })
})
