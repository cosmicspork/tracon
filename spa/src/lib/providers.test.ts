import { expect, test } from 'bun:test'
import { completionInstruction, providerLabel } from './providers'

test('provider labels distinguish API keys from Codex subscriptions', () => {
  expect(providerLabel('anthropic')).toBe('Anthropic')
  expect(providerLabel('openai')).toBe('OpenAI API')
  expect(providerLabel('openai-codex')).toBe('OpenAI Codex')
  expect(providerLabel('custom')).toBe('custom')
})

test('completion copy distinguishes automatic callback from paste', () => {
  expect(completionInstruction('local_callback')).toContain('update automatically')
  expect(completionInstruction('paste')).toContain('paste the redirect URL or code')
  expect(completionInstruction(null)).toContain('paste the redirect URL or code')
})
