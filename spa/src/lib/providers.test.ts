import { expect, test } from 'bun:test'
import {
  completionInstruction,
  connectableProviders,
  credentialImportToml,
  normalizeProviderName,
  providerLabel,
} from './providers'
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

test('a provider is only a row once something has actually happened to it', () => {
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
  // An untouched loginable provider stays out of the list; the "Add a
  // provider" chooser is the only thing that starts a sign-in now, so a
  // never-started subscription provider has nothing to show. An API-key
  // provider has no such untouched state to hide — it exists the moment it
  // is created, disconnected or not, and there is no connect step for it.
  const listed = connectableProviders([
    provider('anthropic', true, 'disconnected'),
    provider('openai', false, 'disconnected'),
    provider('openai-codex', true, 'failed'),
  ])
  expect(listed.map((p) => p.name)).toEqual(['openai', 'openai-codex'])
  expect(connectableProviders([provider('openai', false, 'connected')]).map((p) => p.name)).toEqual(['openai'])
  expect(connectableProviders([provider('anthropic', true, 'pending')]).map((p) => p.name)).toEqual(['anthropic'])
})

test('provider names are normalized to the lowercase id the picker uses', () => {
  expect(normalizeProviderName('  OpenRouter  ')).toBe('openrouter')
  expect(normalizeProviderName('OpenRouter')).toBe('openrouter')
})

test('the credential import TOML seals the key under env.API_KEY, quoted against injection', () => {
  const toml = credentialImportToml('openrouter', 'sk-or-"quote"-and-\\backslash', ['work', 'personal'])
  expect(toml).toContain('[credentials."openrouter"]')
  expect(toml).toContain('kind = "api_key"')
  expect(toml).toContain('channels = ["work", "personal"]')
  expect(toml).toContain('"API_KEY"')
  // Round-trips through TOML's own basic-string escaping, the same subset
  // JSON.stringify produces for a quote and a backslash.
  expect(toml).toContain('\\"quote\\"')
  expect(toml).toContain('\\\\backslash')
})
