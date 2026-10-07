import { expect, test } from 'bun:test'
import { exhaustionDefaults, exhaustionPatch, modelLabel, modelPatch, phaseDefaults } from './bindings'

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
