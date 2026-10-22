// What a channel decides for a phase, so the operator does not decide it at
// every start. The node reads the same keys — `phases.<phase>.model` and
// `phases.<phase>.budget_tokens` — when a session names neither.

import type {
  ChannelBindings,
  ExhaustionChoice,
  ExhaustionPolicy,
  PhaseBinding,
  ReviewTrigger,
  ReviewTriggers,
} from './types'

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

const TRIGGERS: ReviewTrigger[] = ['auto', 'manual']

/** Where a submission comes from, which a channel sets its review trigger for. */
export type ReviewSource = 'session' | 'external'

/**
 * Whether each source's submissions start a reviewer on their own. The node
 * says, defaults included; from one that predates that, the bindings are
 * read with the defaults it would have used: a session's submission starts
 * one, an external harness's waits for you.
 */
export function reviewTriggers(channel: {
  bindings?: ChannelBindings | Record<string, unknown> | null
  review_trigger?: ReviewTriggers
}): ReviewTriggers {
  if (channel.review_trigger) return channel.review_trigger
  const t = (channel.bindings as ChannelBindings | undefined)?.phases?.review?.trigger
  const of = (source: ReviewSource, fallback: ReviewTrigger): ReviewTrigger => {
    if (typeof t === 'string') return TRIGGERS.includes(t) ? t : fallback
    const v = t?.[source]
    return v && TRIGGERS.includes(v) ? v : fallback
  }
  return { session: of('session', 'auto'), external: of('external', 'manual') }
}

/**
 * The patch that sets one source's review trigger. Only that key is
 * written, so the other keeps following the node's default; a single value
 * written for both from the command line is split into the two first.
 */
export function triggerPatch(
  bindings: ChannelBindings | Record<string, unknown> | undefined | null,
  source: ReviewSource,
  trigger: ReviewTrigger,
): Record<string, unknown> {
  const t = (bindings as ChannelBindings | undefined)?.phases?.review?.trigger
  if (typeof t === 'string' && TRIGGERS.includes(t)) {
    return { 'phases.review.trigger': { session: t, external: t, [source]: trigger } }
  }
  // Anything else that is not an object of the two (a value the node does
  // not know, as it reads it, is its default) is replaced whole: a key
  // under it would be written into a string.
  if (t !== undefined && t !== null && (typeof t !== 'object' || Array.isArray(t))) {
    return { 'phases.review.trigger': { [source]: trigger } }
  }
  return { [`phases.review.trigger.${source}`]: trigger }
}
