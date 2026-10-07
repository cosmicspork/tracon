// What a channel decides for a phase, so the operator does not decide it at
// every start. The node reads the same keys — `phases.<phase>.model` and
// `phases.<phase>.budget_tokens` — when a session names neither.

import type { ChannelBindings, ExhaustionChoice, ExhaustionPolicy, PhaseBinding } from './types'

/** The model and budget a channel binds to one phase. */
export function phaseDefaults(
  bindings: ChannelBindings | Record<string, unknown> | undefined | null,
  phase: string,
): PhaseBinding {
  const phases = (bindings as ChannelBindings | undefined)?.phases
  const b = phases?.[phase]
  if (!b) return {}
  const model = typeof b.model === 'string' && b.model.trim() !== '' ? b.model : undefined
  const budget = typeof b.budget_tokens === 'number' && b.budget_tokens > 0 ? b.budget_tokens : undefined
  return { model, budget_tokens: budget }
}

/** The name a model is known by, for a context line that has to stay short. */
export function modelLabel(value: string | undefined, models: { name: string; value: string }[]): string | null {
  if (!value) return null
  return models.find((m) => m.value === value)?.name ?? (value.split('/').at(-1) ?? value)
}

/**
 * The patch that writes one phase's model. An empty choice removes the key —
 * the node's dotted-path merge treats null as a delete.
 */
export function modelPatch(phase: string, model: string): Record<string, unknown> {
  return { [`phases.${phase}.model`]: model.trim() === '' ? null : model.trim() }
}

const POLICIES: ExhaustionPolicy[] = ['pause', 'fallback', 'fallback_then_wait']

/** What a channel does when its provider is exhausted; pause when it says nothing. */
export function exhaustionDefaults(bindings: ChannelBindings | Record<string, unknown> | undefined | null): ExhaustionChoice {
  const e = (bindings as ChannelBindings | undefined)?.exhaustion
  const policy = e && POLICIES.includes(e.policy) ? e.policy : 'pause'
  const fallback = typeof e?.fallback === 'string' && e.fallback.trim() !== '' ? e.fallback : undefined
  return { policy, fallback }
}

/**
 * The patch that writes a channel's exhaustion policy, or `null` when it
 * cannot be written yet: a fallback policy needs its model first, and the
 * node refuses sessions on a channel bound to one without it. Plain pause is
 * the default, so it removes the key.
 */
export function exhaustionPatch(policy: ExhaustionPolicy, fallback: string | undefined): Record<string, unknown> | null {
  const model = fallback?.trim() ?? ''
  if (policy === 'pause') return { exhaustion: null }
  if (model === '') return null
  return { exhaustion: { policy, fallback: model } }
}
