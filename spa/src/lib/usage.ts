// The session's usage line. Two sources, one ledger: the gateway's on-the-wire
// count is what the budget was charged, the harness's own number is shown
// beside it, and a turn that disagreed or could not be counted says so rather
// than reading as an ordinary one.

import { formatTokens } from './format'
import type { SessionUsage } from './types'

export interface UsageNote {
  warn: boolean
  text: string
  title: string
}

export function usageNote(usage: SessionUsage | null): UsageNote | null {
  if (!usage || usage.state === null) return null
  // Cache reads are the model rereading context it already paid for: shown,
  // so a long session's context is visible, and never part of the charge.
  const cached = usage.cache_read_tokens ? ` · cached ${formatTokens(usage.cache_read_tokens)}` : ''
  // Only a harness that reports each request (the native one) is matched
  // request by request; the others carry null and say nothing here.
  const matched = usage.requests_matched ?? null
  const requests =
    matched !== null ? ` · ${matched} of ${matched + (usage.requests_unmatched ?? 0)} requests matched` : ''
  const both = `gw ${formatTokens(usage.gateway_tokens)} · harness ${formatTokens(usage.harness_tokens)}${cached}`
  if (usage.unmetered_turns > 0)
    return {
      warn: true,
      text: `${both} · ${usage.unmetered_turns} unmetered${requests}`,
      title: `${usage.unmetered_turns} turn(s) spent tokens this node could not count: the provider returned no usage. Not zero — unknown.`,
    }
  if (usage.mismatched_turns > 0)
    return {
      warn: true,
      text: `${both} · ${usage.mismatched_turns} mismatched${requests}`,
      title: 'The gateway and the harness disagreed on a turn. The budget was charged the gateway count.',
    }
  return {
    warn: false,
    text: `${both} · reconciled${requests}`,
    title: 'The two usage sources agree within tolerance.',
  }
}
