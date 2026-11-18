import { expect, test } from 'bun:test'
import {
  completionInstruction,
  connectableProviders,
  credentialImportToml,
  disconnectLoses,
  normalizeProviderName,
  providerLabel,
  renewalLines,
  sharedSignIns,
  signIns,
  takesOver,
} from './providers'
import type { NodeInfo, ProviderInfo, ProviderSignIn } from './types'

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
  // ...unless only the node's built-in defaults declare it: then there is
  // nobody's provider to key, and "API key / custom" is the way in.
  const builtIn = { ...provider('openai', false, 'disconnected'), declared: false }
  const declared = { ...provider('openrouter', false, 'disconnected'), declared: true }
  expect(connectableProviders([builtIn, declared]).map((p) => p.name)).toEqual(['openrouter'])
  expect(connectableProviders([{ ...builtIn, state: 'failed' }]).map((p) => p.name)).toEqual(['openai'])
  expect(connectableProviders([{ ...builtIn, state: 'connected' }]).map((p) => p.name)).toEqual(['openai'])
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
  expect(toml).toContain('provider = "openrouter"')
  expect(toml).toContain('channels = ["work", "personal"]')
  expect(toml).toContain('"API_KEY"')
  // Round-trips through TOML's own basic-string escaping, the same subset
  // JSON.stringify produces for a quote and a backslash.
  expect(toml).toContain('\\"quote\\"')
  expect(toml).toContain('\\\\backslash')
})

test('a sign-in for other channels names what it takes over, and a disconnect what loses the provider', () => {
  const signIn = (credential: string, identity: string, channels: string[], extra: Partial<ProviderSignIn> = {}): ProviderSignIn => ({
    credential,
    state: 'connected',
    kind: 'oauth',
    identity,
    expires_ms: null,
    channels,
    bound: channels,
    shared_with: [],
    ...extra,
  })
  const personal = signIn('openai-codex', 'me@example.com', ['personal', 'ops'], { shared_with: ['b0', 'h0'] })
  const work = signIn('openai-codex@work', 'me@work.example', ['work'])
  const codex: ProviderInfo = {
    name: 'openai-codex',
    state: 'connected',
    kind: 'oauth',
    can_login: true,
    identity: 'me@example.com',
    expires_ms: null,
    channels: ['personal', 'ops', 'work'],
    updated_ms: null,
    sign_ins: [personal, work],
  }
  const names: Record<string, string> = { b0: 'bazzite', h0: 'homelab' }
  const name = (id: string) => names[id] ?? id

  expect(takesOver(codex, ['lab'])).toBeNull()
  expect(takesOver(codex, ['work'])).toBe(
    'work is served by me@work.example. This sign-in takes it over; every other channel keeps the sign-in it has.',
  )
  expect(takesOver(codex, ['work', 'personal', 'ops'])).toBe(
    'personal, ops are served by me@example.com; work is served by me@work.example. This sign-in takes them over; every other channel keeps the sign-in it has.',
  )

  expect(disconnectLoses(codex, personal, name)).toBe(
    'Disconnecting me@example.com takes OpenAI Codex from personal, ops on this node; the shared copies on bazzite, homelab stay there.',
  )
  expect(disconnectLoses(codex, work, name)).toBe('Disconnecting me@work.example takes OpenAI Codex from work on this node.')
  // A channel another sign-in is also bound to is not lost.
  const copy = signIn('openai-codex', 'peer@example.com', ['personal'], { bound: ['personal', 'work'] })
  expect(disconnectLoses({ ...codex, sign_ins: [copy, work] }, work, name)).toBeNull()
  // An unbound sign-in kept only here is removed without a question.
  expect(disconnectLoses(codex, signIn('openai-codex@x', 'x@example.com', []), name)).toBeNull()
})

test('a node too old to list sign-ins describes its one', () => {
  const old: ProviderInfo = {
    name: 'anthropic',
    state: 'connected',
    kind: 'oauth',
    can_login: true,
    identity: 'me@example.com',
    expires_ms: 5,
    channels: ['personal'],
    updated_ms: null,
  }
  expect(signIns(old)).toEqual([
    {
      credential: '',
      state: 'connected',
      kind: 'oauth',
      identity: 'me@example.com',
      expires_ms: 5,
      channels: ['personal'],
      bound: ['personal'],
      shared_with: undefined,
    },
  ])
  expect(signIns({ ...old, state: 'pending' })).toEqual([])
  expect(signIns({ ...old, sign_ins: [] })).toEqual([])
})

const oauthSignIn = (extra: Partial<ProviderSignIn> = {}): ProviderSignIn => ({
  credential: 'anthropic',
  state: 'connected',
  kind: 'oauth',
  identity: 'op@example.com',
  expires_ms: 1,
  channels: ['personal'],
  bound: ['personal'],
  ...extra,
})

test('a sign-in says what it waits on, who last renewed it, and why it last failed', () => {
  const name = (id: string) => ({ h: 'homelab', b: 'bazzite' })[id] ?? id
  const at = (ms: number) => `t${ms}`
  expect(renewalLines(oauthSignIn(), name, at)).toEqual([])
  expect(
    renewalLines(
      oauthSignIn({
        renewer: 'h',
        renewal: { state: 'waiting', renewer: null, since_ms: 1 },
        last_renewed: { by: 'h', at_ms: 845, version: 41 },
      }),
      name,
      at,
    ),
  ).toEqual(['Waiting for homelab to renew.', 'Last renewed by homelab at t845 (version 41).'])
  expect(
    renewalLines(
      oauthSignIn({
        renewal: { state: 'recovering', renewer: 'h', since_ms: 1 },
        last_failure: { reason: 'invalid_grant', at_ms: 810 },
      }),
      name,
      at,
    ),
  ).toEqual(['This copy could not be renewed; asking homelab for the current one.', 'Last failed at t810: invalid_grant'])
  expect(renewalLines(oauthSignIn({ renewal: { state: 'waiting', renewer: null, since_ms: 1 } }), name, at)).toEqual([
    'Waiting for another holder to renew.',
  ])
})

test('shared sign-ins are those several nodes hold, with the renewer they name', () => {
  const node = (id: string, is_self: boolean, signIn: ProviderSignIn | null) =>
    ({
      id,
      name: id,
      is_self,
      providers: [
        {
          name: 'anthropic',
          state: signIn ? 'connected' : 'disconnected',
          kind: signIn ? 'oauth' : null,
          can_login: true,
          identity: null,
          expires_ms: null,
          channels: [],
          updated_ms: null,
          sign_ins: signIn ? [signIn] : [],
        },
      ],
    }) as unknown as NodeInfo
  const nodes = [
    node('b', false, oauthSignIn({ renewer: null })),
    node('a', true, oauthSignIn({ renewer: null })),
    node('c', false, null),
  ]
  expect(sharedSignIns(nodes)).toEqual([
    { provider: 'anthropic', credential: 'anthropic', identity: 'op@example.com', holders: ['a', 'b'], renewer: null },
  ])
  nodes[0] = node('b', false, oauthSignIn({ renewer: 'b' }))
  expect(sharedSignIns(nodes)[0].renewer).toBe('b')
  // One node alone is not shared.
  expect(sharedSignIns([nodes[1], nodes[2]])).toEqual([])
})
