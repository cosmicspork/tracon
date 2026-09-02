// What a channel decides for a phase, so the operator does not decide it at
// every start. The node reads the same keys — `phases.<phase>.model` and
// `phases.<phase>.budget_tokens` — when a session names neither.

import type { ChannelBindings, PhaseBinding } from './types'

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
