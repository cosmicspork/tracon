import { expect, test } from 'bun:test'
import { completionInstruction, connectableProviders, providerLabel } from './providers'
import type { ProviderInfo } from './types'

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

test('API-key-only providers are listed only once connected', () => {
  const provider = (name: string, can_login: boolean, state: ProviderInfo['state']): ProviderInfo => ({
    name,
    state,
    kind: null,
    can_login,
    identity: null,
    expires_ms: null,
    channels: [],
    updated_ms: null,
  })
  const listed = connectableProviders([
    provider('anthropic', true, 'disconnected'),
    provider('openai', false, 'disconnected'),
    provider('openai-codex', true, 'failed'),
  ])
  expect(listed.map((p) => p.name)).toEqual(['anthropic', 'openai-codex'])
  expect(connectableProviders([provider('openai', false, 'connected')]).map((p) => p.name)).toEqual(['openai'])
})
