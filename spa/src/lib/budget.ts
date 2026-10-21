// A session that spends its token budget is held, not ended: the node pauses
// it at the end of the turn that spent it, and raising the budget is what lets
// it carry on. Resuming it as it is would only hold it again.

import { formatTokens } from './format'
import type { Session } from './types'

type Spend = Pick<Session, 'state' | 'budget_tokens' | 'tokens_used'>

/** Paused with its budget spent: the way on is a higher budget, not Resume. */
export function heldAtBudget(s: Spend): boolean {
  return s.state === 'paused' && s.budget_tokens > 0 && s.tokens_used >= s.budget_tokens
}

/**
 * What the raise box offers first: double the budget, or the budget again on
 * top of what was spent when one long turn overshot by more than that. With
 * no budget, twice what was spent, so the offer is never one the node refuses.
 */
export function suggestedBudget(s: Pick<Session, 'budget_tokens' | 'tokens_used'>): number {
  if (s.budget_tokens <= 0) return Math.max(s.tokens_used * 2, 1_000_000)
  return Math.max(s.budget_tokens * 2, s.tokens_used + s.budget_tokens)
}

/** Why a new budget would be refused, or null: the node takes 0 (none) or more than was spent. */
export function budgetProblem(value: number, used: number): string | null {
  if (!Number.isInteger(value) || value < 0) return 'a whole number of tokens, or 0 for none'
  if (value > 0 && value <= used) return `more than the ${formatTokens(used)} tokens already spent`
  return null
}
