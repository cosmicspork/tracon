import type { LoginCompletion, ProviderInfo } from './types'

export function providerLabel(name: string): string {
  if (name === 'openai') return 'OpenAI API'
  if (name === 'openai-codex') return 'OpenAI Codex'
  if (name === 'anthropic') return 'Anthropic'
  return name
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
// failed attempt to retry, or a live connection. A loginable provider that
// has never been touched is not shown disconnected — the Connections pane's
// "Add a provider" chooser is the only entry point that starts one, so an
// entry that has not been started stays out of the row list entirely rather
// than sitting there as a card with nothing on it but a Connect button.
export function connectableProviders(providers: ProviderInfo[]): ProviderInfo[] {
  return providers.filter((provider) => provider.state !== 'disconnected')
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
    `channels = [${channelList}]`,
    `env = { ${quote(CREDENTIAL_KEY_ENV)} = ${quote(key)} }`,
    '',
  ].join('\n')
}
