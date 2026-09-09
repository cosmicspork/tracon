import { expect, test } from 'bun:test'
import { filterModels, matchesQuery, orderModels, recentModelValues } from './models'
import type { Session } from './types'

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
