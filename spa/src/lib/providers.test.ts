import { expect, test } from 'bun:test'
import {
  completionInstruction,
  connectableProviders,
  credentialImportToml,
  disconnectLoses,
  normalizeProviderName,
  providerCardHref,
  providerLabel,
  renewalLines,
  sharedSignIns,
  signInAgain,
  signInAgainTakesOver,
  signIns,
  signInsNeeded,
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

test('a sign-in that can no longer be renewed offers signing in again, here or on its peer', () => {
  const p: ProviderInfo = {
    name: 'anthropic',
    state: 'connected',
    kind: 'oauth',
    can_login: true,
    sign_in_again: true,
    identity: null,
    expires_ms: null,
    channels: ['work'],
    updated_ms: null,
  }
  const row = (state: ProviderSignIn['state'], extra: Partial<ProviderSignIn> = {}): ProviderSignIn => ({
    credential: 'anthropic',
    state,
    kind: 'oauth',
    identity: 'me@example.com',
    expires_ms: null,
    channels: ['work'],
    bound: ['work'],
    ...extra,
  })
  // Connected: nothing to do, on either card.
  expect(signInAgain(p, row('connected'), true, 'bazzite')).toBeNull()
  expect(signInAgain(p, row('connected'), false, 'homelab')).toBeNull()
  // This node's card.
  expect(signInAgain(p, row('needs_reconnect'), true, 'bazzite')).toEqual({ kind: 'here', label: 'Sign in again' })
  expect(signInAgain(p, row('failed'), true, 'bazzite')).toEqual({ kind: 'here', label: 'Sign in again' })
  // A peer's card: run on the peer, named.
  expect(signInAgain(p, row('needs_reconnect'), false, 'homelab')).toEqual({
    kind: 'peer',
    label: 'Sign in again on homelab',
  })
  // A peer that does not take it says where to go instead of failing.
  const older: ProviderInfo = { ...p, sign_in_again: undefined }
  expect(signInAgain(older, row('needs_reconnect'), false, 'homelab')).toEqual({
    kind: 'elsewhere',
    text: "Open homelab's own UI to sign in again.",
  })
  expect(signInAgain(p, row('needs_reconnect', { credential: '' }), false, 'homelab')?.kind).toBe('elsewhere')
  // An API key takes a new key in place; a provider with no login has none.
  expect(signInAgain(p, row('needs_reconnect', { kind: 'api_key' }), true, 'bazzite')).toBeNull()
  expect(signInAgain({ ...p, can_login: false }, row('needs_reconnect'), true, 'bazzite')).toBeNull()
})

test('signing in again for a shared copy names the nodes it then shares with', () => {
  const names: Record<string, string> = { b: 'bazzite', h: 'homelab', m: 'mac' }
  const name = (id: string) => names[id] ?? id
  const copy: ProviderSignIn = {
    credential: 'anthropic',
    state: 'needs_reconnect',
    kind: 'oauth',
    identity: null,
    expires_ms: null,
    channels: ['work'],
    bound: ['work'],
    shared_with: ['b', 'm'],
    origin: 'b',
  }
  expect(signInAgainTakesOver(copy, 'h', 'homelab', name)).toBe(
    "This is a copy bazzite shared. Signing in again here makes homelab the sign-in's origin; it then shares it with bazzite, mac.",
  )
  expect(signInAgainTakesOver({ ...copy, shared_with: [] }, 'h', 'homelab', name)).toContain('no other node holds it')
  // Its own sign-in, or one from a node too old to say: nothing to confirm.
  expect(signInAgainTakesOver({ ...copy, origin: 'h' }, 'h', 'homelab', name)).toBeNull()
  expect(signInAgainTakesOver({ ...copy, origin: undefined }, 'h', 'homelab', name)).toBeNull()
})

test('the banner names each provider whose sign-in a channel still uses and cannot renew', () => {
  const p = (state: ProviderSignIn['state'], channels: string[]): ProviderInfo => ({
    name: 'anthropic',
    state: 'connected',
    kind: 'oauth',
    can_login: true,
    identity: null,
    expires_ms: null,
    channels,
    updated_ms: null,
    sign_ins: [{ credential: 'anthropic', state, kind: 'oauth', identity: null, expires_ms: null, channels, bound: ['work'] }],
  })
  const node = (id: string, name: string, extra: Partial<NodeInfo>): NodeInfo => ({ id, name, ...extra }) as NodeInfo
  const nodes = [
    node('b', 'bazzite', { is_self: true, reachable: true, providers: [p('connected', ['work'])] }),
    node('h', 'homelab', { is_self: false, reachable: true, providers: [p('needs_reconnect', ['work'])] }),
    node('m', 'mac', { is_self: false, reachable: false, providers: [p('needs_reconnect', ['work'])] }),
    node('x', 'spare', { is_self: false, reachable: true, providers: [p('needs_reconnect', [])] }),
  ]
  // The serving node reads its own listing, not the one its hello carries.
  expect(signInsNeeded(nodes, [p('needs_reconnect', ['work'])])).toEqual([
    {
      provider: 'anthropic',
      nodes: [
        { id: 'b', name: 'bazzite' },
        { id: 'h', name: 'homelab' },
      ],
    },
  ])
  expect(signInsNeeded(nodes, [p('connected', ['work'])])).toEqual([{ provider: 'anthropic', nodes: [{ id: 'h', name: 'homelab' }] }])
  expect(signInsNeeded(nodes.slice(0, 1), [p('connected', ['work'])])).toEqual([])
  expect(providerCardHref('h')).toBe('/settings?node=h#connections')
})
