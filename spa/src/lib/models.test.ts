import { expect, test } from 'bun:test'
import { filterModels, matchesQuery, modelSummary, orderModels, recentModelValues } from './models'
import type { ProviderInfo, Session } from './types'

const models = [
  { value: 'openai-codex/gpt-5.6-sol', name: 'GPT-5.6-Sol' },
  { value: 'anthropic/claude-opus-5', name: 'Claude Opus 5' },
  { value: 'anthropic/claude-sonnet-5', name: 'Claude Sonnet 5' },
]

function session(model: string, created_ms: number): Session {
  return { model, created_ms } as Session
}

test('recent models are the ones sessions ran, newest first, without repeats', () => {
  const recent = recentModelValues([
    session('anthropic/claude-sonnet-5', 10),
    session('anthropic/claude-opus-5', 30),
    session('anthropic/claude-sonnet-5', 20),
    session('', 40),
  ])
  expect(recent).toEqual(['anthropic/claude-opus-5', 'anthropic/claude-sonnet-5'])
})

test('ordering puts used models first and keeps source order for the rest', () => {
  const { recent, rest } = orderModels(models, ['anthropic/claude-sonnet-5', 'gone/model'])
  expect(recent.map((m) => m.value)).toEqual(['anthropic/claude-sonnet-5'])
  expect(rest.map((m) => m.value)).toEqual(['openai-codex/gpt-5.6-sol', 'anthropic/claude-opus-5'])
})

test('filtering matches every word against name and value', () => {
  expect(filterModels(models, '').length).toBe(3)
  expect(filterModels(models, 'claude 5').map((m) => m.name)).toEqual(['Claude Opus 5', 'Claude Sonnet 5'])
  expect(filterModels(models, 'codex').map((m) => m.name)).toEqual(['GPT-5.6-Sol'])
  expect(filterModels(models, 'opus sonnet')).toEqual([])
  expect(matchesQuery('GitLab group/sub/project', 'SUB proj')).toBe(true)
})

function provider(name: string): ProviderInfo {
  return { name, state: 'connected' } as ProviderInfo
}

const connected = [provider('anthropic'), provider('openai-codex')]

test('a probe is attributed to the providers that could have served it', () => {
  expect(modelSummary(models, connected)).toBe('3 models · Anthropic 2 · OpenAI Codex 1')
})

test('providers lead in their own order, so a second probe reads like the first', () => {
  const reversed = [...models].reverse()
  expect(modelSummary(reversed, connected)).toBe(modelSummary(models, connected))
})

test('an alias no provider name explains is counted apart, never guessed at', () => {
  const withAliases = [
    ...models,
    { value: 'sonnet', name: 'Sonnet' },
    { value: 'mystery/thing', name: 'Thing' },
  ]
  expect(modelSummary(withAliases, connected)).toBe(
    '5 models · Anthropic 2 · OpenAI Codex 1 · 2 adapter aliases',
  )
})

test('one of each reads as one, not as a bare plural', () => {
  const one = [{ value: 'anthropic/claude-opus-5', name: 'Claude Opus 5' }, { value: 'sonnet', name: 'Sonnet' }]
  expect(modelSummary(one, connected)).toBe('2 models · Anthropic 1 · 1 adapter alias')
  expect(modelSummary([one[0]], connected)).toBe('1 model · Anthropic 1')
})

test('an empty catalogue says the probe worked and the harness had nothing', () => {
  expect(modelSummary([], connected)).toBe('the harness listed no models through the gateway')
})

test('a provider with no models of its own is left out of the breakdown', () => {
  const summary = modelSummary(models, [...connected, provider('openai')])
  expect(summary).toBe('3 models · Anthropic 2 · OpenAI Codex 1')
})
