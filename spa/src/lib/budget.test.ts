import { expect, test } from 'bun:test'
import { budgetProblem, heldAtBudget, suggestedBudget } from './budget'

test('a paused session with its budget spent is held at it', () => {
  expect(heldAtBudget({ state: 'paused', budget_tokens: 1000, tokens_used: 1500 })).toBe(true)
  expect(heldAtBudget({ state: 'paused', budget_tokens: 1000, tokens_used: 1000 })).toBe(true)
  // Paused for another reason, with room left; or no budget at all.
  expect(heldAtBudget({ state: 'paused', budget_tokens: 1000, tokens_used: 999 })).toBe(false)
  expect(heldAtBudget({ state: 'paused', budget_tokens: 0, tokens_used: 1500 })).toBe(false)
  // An old session the node ended at its budget is not held: nothing resumes it.
  expect(heldAtBudget({ state: 'killed_budget', budget_tokens: 1000, tokens_used: 1500 })).toBe(false)
})

test('the box offers double the budget, and room past an overshoot', () => {
  expect(suggestedBudget({ budget_tokens: 2_000_000, tokens_used: 2_100_000 })).toBe(4_100_000)
  expect(suggestedBudget({ budget_tokens: 1000, tokens_used: 1000 })).toBe(2000)
  expect(suggestedBudget({ budget_tokens: 1000, tokens_used: 5000 })).toBe(6000)
  // No budget yet: something above what was spent.
  expect(suggestedBudget({ budget_tokens: 0, tokens_used: 3_000_000 })).toBe(6_000_000)
  expect(suggestedBudget({ budget_tokens: 0, tokens_used: 0 })).toBe(1_000_000)
})

test('a budget the node would refuse says why before it is sent', () => {
  expect(budgetProblem(2000, 1500)).toBeNull()
  expect(budgetProblem(0, 1500)).toBeNull()
  expect(budgetProblem(1500, 1500)).toBe('more than the 2k tokens already spent')
  expect(budgetProblem(-1, 0)).toBe('a whole number of tokens, or 0 for none')
  expect(budgetProblem(1.5, 0)).toBe('a whole number of tokens, or 0 for none')
})
