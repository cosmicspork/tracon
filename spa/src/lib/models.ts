// Ordering and filtering a model list for a picker. The harness reports its
// models in its own order and says nothing about their age, so the one
// recency this node knows is which of them it has run: those come first,
// newest use first, and the rest keep the order they arrived in.

import type { ModelOption, Session } from './types'

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
