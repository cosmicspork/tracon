// Ordering and filtering a model list for a picker. The harness reports its
// models in its own order and says nothing about their age, so the one
// recency this node knows is which of them it has run: those come first,
// newest use first, and the rest keep the order they arrived in.

import type { ModelOption, ProviderInfo, Session } from './types'
import { providerLabel } from './providers'

/** Distinct model values from sessions on this node, most recently started first. */
export function recentModelValues(sessions: Iterable<Session>): string[] {
  const seen = new Set<string>()
  return [...sessions]
    .filter((s) => s.model)
    .sort((a, b) => b.created_ms - a.created_ms)
    .map((s) => s.model)
    .filter((m) => (seen.has(m) ? false : (seen.add(m), true)))
}

/** The models split into the ones used here (in `recent` order) and the rest (source order). */
export function orderModels(
  models: ModelOption[],
  recent: string[],
): { recent: ModelOption[]; rest: ModelOption[] } {
  const byValue = new Map(models.map((m) => [m.value, m]))
  const used = recent.map((v) => byValue.get(v)).filter((m): m is ModelOption => m !== undefined)
  const usedValues = new Set(used.map((m) => m.value))
  return { recent: used, rest: models.filter((m) => !usedValues.has(m.value)) }
}

/** Every word of the query appears in the text, case-insensitively. */
export function matchesQuery(text: string, query: string): boolean {
  const hay = text.toLowerCase()
  return query
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .every((word) => hay.includes(word))
}

export function filterModels(models: ModelOption[], query: string): ModelOption[] {
  if (query.trim() === '') return models
  return models.filter((m) => matchesQuery(`${m.name} ${m.value}`, query))
}

/** What a probe came back with, attributed to the providers that could have
    served it: `12 models · Anthropic 8 · OpenAI Codex 3 · 1 adapter alias`.
    A model belongs to the provider named before its first `/`; an adapter-owned
    alias carries no provider it can be reverse-engineered from, so it is
    counted apart rather than guessed at. Providers lead in their own order, not
    the harness's, so a second probe reads the same way as the first.

    A CONNECTED provider that contributed nothing is named at zero, because that
    is the whole point of counting per provider: a credential that signs in and
    still offers no model is the failure this line exists to show, and it would
    otherwise be indistinguishable from a provider that is simply absent. A
    provider that was never connected is left out — it has no credential to
    answer with, so its nothing means nothing. */
export function modelSummary(models: ModelOption[], providers: ProviderInfo[]): string {
  if (models.length === 0) return 'the harness listed no models through the gateway'
  const known = new Set(providers.map((provider) => provider.name))
  const counts = new Map<string, number>()
  let aliases = 0
  for (const model of models) {
    const slash = model.value.indexOf('/')
    const name = slash < 0 ? null : model.value.slice(0, slash)
    if (name === null || !known.has(name)) aliases += 1
    else counts.set(name, (counts.get(name) ?? 0) + 1)
  }
  const parts = providers
    .filter((provider) => counts.has(provider.name) || provider.state === 'connected')
    .map((provider) => `${providerLabel(provider.name)} ${counts.get(provider.name) ?? 0}`)
  if (aliases > 0) parts.push(`${aliases} adapter alias${aliases === 1 ? '' : 'es'}`)
  return [`${models.length} model${models.length === 1 ? '' : 's'}`, ...parts].join(' · ')
}
