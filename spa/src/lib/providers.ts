import type { LoginCompletion, ProviderInfo, ProviderSignIn } from './types'

export function providerLabel(name: string): string {
  if (name === 'openai') return 'OpenAI API'
  if (name === 'openai-codex') return 'OpenAI Codex'
  if (name === 'anthropic') return 'Anthropic'
  return name
}

/** The sign-ins a provider holds on its node, or for a node too old to
    list them, the one its top-level fields describe. */
export function signIns(p: ProviderInfo): ProviderSignIn[] {
  if (p.sign_ins) return p.sign_ins
  if (p.state !== 'connected' || p.kind === null) return []
  return [
    {
      credential: '',
      state: 'connected',
      kind: p.kind,
      identity: p.identity,
      expires_ms: p.expires_ms,
      channels: p.channels,
      bound: p.channels,
      shared_with: p.shared_with,
    },
  ]
}

/** How the card names a sign-in: the account it is, or failing that the
    credential it is held under. */
export function signInLabel(signIn: ProviderSignIn): string {
  return signIn.identity ?? (signIn.credential || 'this sign-in')
}

/**
 * Which of `picked` a new sign-in would take over from a sign-in already
 * serving them on the node, in the words the card shows beside Connect.
 * Nothing outside `picked` changes: the node keeps every other channel on
 * the sign-in it has. Null when none of them is served yet.
 */
export function takesOver(p: ProviderInfo, picked: string[]): string | null {
  const parts = signIns(p)
    .map((signIn) => ({ signIn, taken: signIn.channels.filter((channel) => picked.includes(channel)) }))
    .filter(({ taken }) => taken.length)
    .map(({ signIn, taken }) => `${taken.join(', ')} ${taken.length === 1 ? 'is' : 'are'} served by ${signInLabel(signIn)}`)
  if (!parts.length) return null
  return `${parts.join('; ')}. This sign-in takes ${parts.length === 1 && picked.length === 1 ? 'it' : 'them'} over; every other channel keeps the sign-in it has.`
}

/**
 * What disconnecting `signIn` would take from the provider's node, in the
 * words the card asks the operator to confirm: the channels it serves that
 * no other sign-in there is bound to, and the peers whose shared copies
 * stay. Null when nothing is lost. The node refuses the same cases until
 * confirmed (`confirm_displaced`).
 */
export function disconnectLoses(
  p: ProviderInfo,
  signIn: ProviderSignIn,
  peerName: (id: string) => string = (id) => id,
): string | null {
  const others = signIns(p).filter((other) => other !== signIn)
  const lost = signIn.channels.filter((channel) => !others.some((other) => other.bound.includes(channel)))
  const peers = (signIn.shared_with ?? []).map(peerName)
  if (!lost.length && !peers.length) return null
  const label = providerLabel(p.name)
  const taken = lost.length
    ? `Disconnecting ${signInLabel(signIn)} takes ${label} from ${lost.join(', ')} on this node`
    : `Disconnecting ${signInLabel(signIn)} leaves no channel here without ${label}`
  const copies = peers.length ? `; the shared copies on ${peers.join(', ')} stay there` : ''
  return `${taken}${copies}.`
}

export function completionInstruction(completion: LoginCompletion | null): string {
  if (completion === 'local_callback') {
    return 'Complete sign-in in your browser; this page will update automatically.'
  }
  if (completion === 'device_code') {
    return 'Open the provider page and enter the code below.'
  }
  return 'Complete sign-in, then paste the redirect URL or code.'
}

// A card appears once there is something to show: a sign-in in flight, a
// failed attempt to retry, a live connection, or an API-key provider the
// operator declared in node.toml, which takes its key on the card. Hidden
// while disconnected: a subscription nobody has started (the "Add a
// provider" chooser starts one), and an API-key provider only the node's
// built-in defaults declare (`openai` is in every node; "API key / custom"
// adds it). A node too old to say whether it declared one keeps the card.
export function connectableProviders(providers: ProviderInfo[]): ProviderInfo[] {
  return providers.filter(
    (provider) => provider.state !== 'disconnected' || (!provider.can_login && provider.declared !== false),
  )
}

/** The shapes `POST /api/providers` accepts, and what the Shape dropdown offers. */
export const PROVIDER_SHAPES = [
  { value: 'openai', label: 'OpenAI-compatible' },
  { value: 'anthropic', label: 'Anthropic-compatible' },
] as const

/** How the settings pane writes a credential ready to inject: kind `api_key`,
    the key under `env.API_KEY` — the exact name `Broker::injection` reads for
    that kind, regardless of shape (shape only decides whether the gateway
    sends it as `x-api-key` or `Authorization: Bearer`; see `broker::Credential`
    and `Broker::injection` in node/src/broker/mod.rs). Not
    `TRACON_PROVIDER_KEY_<NAME>` — that name is the harness-facing placeholder
    `gateway::model::key_env_name` wires in, a different mechanism entirely. */
export const CREDENTIAL_KEY_ENV = 'API_KEY'

/** The lowercase, trimmed provider id the form's Name field becomes. */
export function normalizeProviderName(raw: string): string {
  return raw.trim().toLowerCase()
}

/** One `[credentials.<name>]` table, TOML the existing `/api/credentials/import`
    endpoint accepts unmodified (see `Broker::parse_text`). Every value is
    written as a quoted TOML string, so a name or key containing a quote or
    backslash round-trips rather than breaking the document. */
export function credentialImportToml(name: string, key: string, channels: string[]): string {
  const quote = (s: string) => JSON.stringify(s)
  const channelList = channels.map(quote).join(', ')
  return [
    `[credentials.${quote(name)}]`,
    'kind = "api_key"',
    // The provider the gateway injects it for, and what Connections reads
    // to call the provider connected.
    `provider = ${quote(name)}`,
    `channels = [${channelList}]`,
    `env = { ${quote(CREDENTIAL_KEY_ENV)} = ${quote(key)} }`,
    '',
  ].join('\n')
}
