import type { LoginCompletion } from './types'

export function providerLabel(name: string): string {
  if (name === 'openai') return 'OpenAI API'
  if (name === 'openai-codex') return 'OpenAI Codex'
  if (name === 'anthropic') return 'Anthropic'
  return name
}

export function completionInstruction(completion: LoginCompletion | null): string {
  return completion === 'local_callback'
    ? 'Complete sign-in in your browser; this page will update automatically.'
    : 'Complete sign-in, then paste the redirect URL or code.'
}
