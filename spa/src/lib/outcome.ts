import type { OutcomeCheck, OutcomeClaim, SessionOutcome } from './types'

/** One line saying where the session stands, worst news first. Built from the
    node's verdicts only; a claim's wording never changes it. */
export function headline(o: SessionOutcome): string {
  const current = o.verified.filter((c) => c.current)
  const failing = current.filter((c) => c.failed).length
  const passing = current.filter((c) => c.passed).length
  const pending = o.needs_decision.length
  const parts: string[] = []
  // Plan and review sessions write a document or a verdict, not code: there
  // is no commit of theirs to verify.
  if (o.phase === 'plan') parts.push(o.plan ? 'plan written' : 'no plan written')
  else if (o.phase === 'review') parts.push(verdictLine(o))
  else if (!o.head_sha) parts.push('nothing submitted for review')
  else if (failing) parts.push(`${failing} check${failing === 1 ? '' : 's'} failing on the latest commit`)
  else if (passing) parts.push(`checks pass on the latest commit`)
  else if (o.owner_checks && !o.owner_checks.read) parts.push('its checks could not be read from the node that runs it')
  else parts.push('the latest commit is unverified')
  if (pending) parts.push(`${pending} waiting on you`)
  if (o.uncertain.length) parts.push(`${o.uncertain.length} uncertain`)
  return parts.join(' · ')
}

/** What a review session concluded, or that it has not. */
export function verdictLine(o: SessionOutcome): string {
  const v = o.verdict
  if (!v) return 'no verdict given'
  const said = v.verdict === 'approve' ? 'approves' : 'requests changes'
  const findings = v.findings ? `, ${v.findings} finding${v.findings === 1 ? '' : 's'}` : ''
  return `${said}${findings}${v.superseded ? ' (superseded)' : ''}`
}

/** What stands in for the checks when there are none to list: a peer's
    session whose node could not be read is not one no check ran on. */
export function noChecks(o: SessionOutcome): string {
  if (o.owner_checks && !o.owner_checks.read) {
    const why = o.owner_checks.reason ? `: ${o.owner_checks.reason}` : ''
    return `Its checks are held by the node that runs this session and could not be read from it${why}.`
  }
  return 'No check has run.'
}

/** How a check reads: a reused run says what its source concluded. */
export function checkLabel(c: OutcomeCheck): string {
  const what = c.command ?? 'check'
  const verdict = c.outcome === 'reused' ? `reused, ${c.source_outcome ?? 'unknown'}` : c.outcome
  const where = c.head_sha ? ` on ${c.head_sha.slice(0, 12)}` : ''
  return `${what}: ${verdict}${where}${c.head_sha && !c.current ? ' (earlier commit)' : ''}`
}

/** Why a claim stands or does not. */
export function claimStanding(c: OutcomeClaim): string {
  if (c.backed) return `backed by ${c.backed_by.length} passing check${c.backed_by.length === 1 ? '' : 's'} on its commit`
  if (c.source === 'report') return 'narrative only; a report has no commit a check could verify'
  return 'the author’s word only; no passing check on its commit'
}
