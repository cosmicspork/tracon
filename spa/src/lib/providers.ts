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

// An API-key-only provider has nothing to connect from its card; its key is
// imported under credentials, and its card appears once one is.
export function connectableProviders(providers: ProviderInfo[]): ProviderInfo[] {
  return providers.filter((provider) => provider.can_login || provider.state === 'connected')
}
