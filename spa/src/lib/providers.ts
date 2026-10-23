import type { LoginCompletion, ProviderInfo } from './types'

export function providerLabel(name: string): string {
  if (name === 'openai') return 'OpenAI API'
  if (name === 'openai-codex') return 'OpenAI Codex'
  if (name === 'anthropic') return 'Anthropic'
  return name
}

/** What the card says about a subscription: the node holds one sign-in for
    it, whichever channels it serves. */
export function oneSignIn(p: ProviderInfo): string {
  return `This node holds one ${providerLabel(p.name)} sign-in, for every channel it serves: signing in again replaces it, and disconnecting removes it, for all of them.`
}

/**
 * What signing in for `keeps` would take from the sign-in a provider already
 * holds on the serving node, or with `null` what disconnecting it would, in
 * the words the card asks the operator to confirm. Null when nothing is lost.
 * The node refuses the same cases until confirmed (`confirm_displaced`):
 * channels the new sign-in does not serve lose the provider here, and a
 * disconnect leaves peers' shared copies where they are.
 */
export function displacedSignIn(
  p: ProviderInfo,
  keeps: string[] | null,
  peerName: (id: string) => string = (id) => id,
): string | null {
  // Only a held credential is replaced, and a disconnect while a sign-in is
  // in flight only cancels that sign-in.
  if (p.kind === null || (keeps === null && p.state === 'pending')) return null
  const label = providerLabel(p.name)
  const lost = p.channels.filter((channel) => keeps === null || !keeps.includes(channel))
  const peers = (p.shared_with ?? []).map(peerName)
  if (keeps !== null) {
    if (!lost.length) return null
    const copies = peers.length ? `; ${peers.join(', ')} ${peers.length === 1 ? 'holds' : 'hold'} a shared copy of it` : ''
    return `${label} holds one sign-in on this node. Signing in for ${keeps.join(', ')} replaces the one serving ${lost.join(', ')}, which ${lost.length === 1 ? 'loses' : 'lose'} it here${copies}.`
  }
  if (!lost.length && !peers.length) return null
  const taken = lost.length ? `Disconnecting takes ${label} from ${lost.join(', ')} on this node` : `Disconnecting removes ${label} from this node`
  const copies = peers.length ? `; the shared copies on ${peers.join(', ')} stay there` : ''
  return `${label} holds one sign-in on this node. ${taken}${copies}.`
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
