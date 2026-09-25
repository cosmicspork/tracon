// Turns the flat event log into what the session screen renders: consecutive
// tool calls fold into one group, open while any of them lacks a result, and a
// permission request breaks the group so it sits at its true position.

import { formatTokens } from './format'
import type { Event } from './types'

export interface ToolEntry {
  call: Event
  result?: Event
  progress?: string
}

export interface LogEntry {
  kind: 'leaf' | 'tools'
  event?: Event
  tools?: ToolEntry[]
}

export function groupLog(events: Event[], progress: Map<string, string> = new Map()): LogEntry[] {
  const out: LogEntry[] = []
  const openCalls = new Map<string, ToolEntry>()

  for (const e of events) {
    if (e.kind === 'tool_call') {
      const entry: ToolEntry = { call: e, progress: progress.get(e.ref_id ?? '') }
      const last = out[out.length - 1]
      if (last?.kind === 'tools') {
        last.tools!.push(entry)
      } else {
        out.push({ kind: 'tools', tools: [entry] })
      }
      if (e.ref_id) openCalls.set(e.ref_id, entry)
      continue
    }
    if (e.kind === 'tool_result') {
      const open = e.ref_id ? openCalls.get(e.ref_id) : undefined
      if (open) {
        open.result = e
        if (e.ref_id) openCalls.delete(e.ref_id)
        continue
      }
      // A result with no visible call still deserves a place in the log.
      out.push({ kind: 'leaf', event: e })
      continue
    }
    // Everything else is a leaf and ends any run of tool calls, so the next
    // tool call starts a new group at its true position.
    out.push({ kind: 'leaf', event: e })
  }
  return out
}

/// Both sides of a usage disagreement on one line: "usage disagrees · gateway
/// 10.0k · harness 12 · charged 10.0k". Never one number: the operator is the
/// one who can say which source is wrong, and they cannot do that from a
/// figure that has already picked a winner.
export function usageMismatchLine(payload: Record<string, unknown>): string {
  const gateway = numberAt(payload, 'gateway', 'tokens')
  const harness = numberAt(payload, 'harness', 'tokens')
  const charged = typeof payload.charged_tokens === 'number' ? payload.charged_tokens : gateway
  const said = harness === null ? 'harness reported nothing' : `harness ${formatTokens(harness)}`
  return `usage disagrees · gateway ${formatTokens(gateway ?? 0)} · ${said} · charged ${formatTokens(charged ?? 0)}`
}

/// "usage unmetered · 3 calls · the provider returned no usage" — a turn that
/// spent something nobody could count. Deliberately not phrased as zero.
export function usageUnmeteredLine(payload: Record<string, unknown>): string {
  const requests = numberAt(payload, 'gateway', 'requests') ?? 0
  return `usage unmetered · ${requests} model ${requests === 1 ? 'call' : 'calls'} · the provider returned no usage, so this turn's cost is unknown rather than zero`
}

function numberAt(payload: Record<string, unknown>, group: string, field: string): number | null {
  const inner = payload[group]
  if (typeof inner !== 'object' || inner === null) return null
  const value = (inner as Record<string, unknown>)[field]
  return typeof value === 'number' ? value : null
}

/// "anthropic answered 429 · Error · harness retrying · attempt 2" — one line
/// for a provider refusal the harness is still retrying through. The session
/// stays running, so this reads as progress being made on the operator's
/// behalf, not as a failure.
export function providerErrorLine(payload: Record<string, unknown>): string {
  const provider = typeof payload.provider === 'string' && payload.provider ? payload.provider : 'the provider'
  const status = typeof payload.status === 'number' ? `answered ${payload.status}` : 'refused the call'
  const message = typeof payload.message === 'string' ? payload.message.trim() : ''
  const attempt = typeof payload.attempt === 'number' && payload.attempt > 1 ? `attempt ${payload.attempt}` : ''
  return [`${provider} ${status}`, message, 'harness retrying', attempt].filter(Boolean).join(' · ')
}

/// "same call 3× in a row · run just test · still running" — the repetition
/// signal. It reads as something to look at, not as a verdict: the node does
/// not pause on repetition, because repeating a command is also what a fix-
/// then-test loop looks like.
export function repetitionLine(payload: Record<string, unknown>): string {
  const count = typeof payload.count === 'number' ? payload.count : 0
  const title = typeof payload.title === 'string' && payload.title ? payload.title : 'the same tool call'
  return `same call ${count}× in a row · ${title} · recorded, not paused`
}

/// The most recent repetition signal of the current turn, if the harness has
/// not moved on from it. Anything else since — a turn ending, a pause, a
/// resume — means the run is over and the hint has nothing left to say.
export function repetitionHint(events: Event[]): string | null {
  for (let i = events.length - 1; i >= 0; i--) {
    const kind = events[i].kind
    if (kind === 'repetition') return repetitionLine(events[i].payload)
    if (kind === 'turn_end' || kind === 'session_paused' || kind === 'session_resumed') return null
  }
  return null
}

export function groupOpen(tools: ToolEntry[]): boolean {
  return tools.some((t) => !t.result)
}

/// "Read 2 files, ran 1 shell command" — the folded summary line.
export function groupSummary(tools: ToolEntry[]): string {
  const counts = new Map<string, number>()
  for (const t of tools) {
    const kind = (t.call.payload.kind as string) ?? 'tool'
    counts.set(kind, (counts.get(kind) ?? 0) + 1)
  }
  const labels: Record<string, [string, string]> = {
    read: ['read %d file', 'read %d files'],
    edit: ['edited %d file', 'edited %d files'],
    execute: ['ran %d shell command', 'ran %d shell commands'],
    think: ['updated the plan', 'updated the plan'],
    fetch: ['fetched %d page', 'fetched %d pages'],
  }
  const parts: string[] = []
  for (const [kind, n] of counts) {
    const [one, many] = labels[kind] ?? [`%d ${kind} call`, `%d ${kind} calls`]
    parts.push((n === 1 ? one : many).replace('%d', String(n)))
  }
  const failed = tools.filter((t) => t.result?.payload.status === 'failed').length
  const text = parts.join(', ')
  const capitalised = text.charAt(0).toUpperCase() + text.slice(1)
  return failed > 0 ? `${capitalised} · ${failed} failed` : capitalised
}

/** One piece of context the node's cap kept out of a session's orientation. */
export interface MissingContext {
  what: string
  partial: boolean
  chars: number
  fetch?: string
}

export function missingContext(payload: Record<string, unknown>): MissingContext[] {
  const raw = payload.missing
  if (!Array.isArray(raw)) return []
  return raw.filter(
    (m): m is MissingContext =>
      !!m && typeof m === 'object' && typeof (m as MissingContext).what === 'string',
  )
}

/// "orientation · 22,812 chars · 4 not included" — the count is the operator's
/// cue to open the fold, where each omission is named. A bare "trimmed" told
/// them something was cut but never what, which is not something anyone can
/// act on.
export function orientationLine(payload: Record<string, unknown>): string {
  const chars = typeof payload.chars === 'number' ? payload.chars : 0
  const missing = missingContext(payload)
  // An event recorded before the node named its omissions has the old flag and
  // nothing else. Keep showing it: a vague signal still beats none, and it
  // marks the event as one whose detail is genuinely unavailable rather than
  // one where nothing was cut.
  const cut = missing.length
    ? ` · ${missing.length} ${missing.length === 1 ? 'piece' : 'pieces'} not included in full`
    : payload.trimmed
      ? ' · trimmed'
      : ''
  const context = orientationContext(payload)
  const ctx = context
    ? ` · context revision ${context.revision}${context.changes.length ? ` · ${context.changes.length} changed` : ''}`
    : ''
  return `orientation · ${formatTokens(chars)} chars${ctx}${cut}`
}

/** The selected context an orientation carried: its revision, and what moved since the previous attempt. */
export interface OrientationContext {
  revision: number
  previous_revision: number | null
  changes: { kind: string; slug: string; says: string }[]
  omitted: string[]
}

export function orientationContext(payload: Record<string, unknown>): OrientationContext | null {
  const raw = payload.context as Partial<OrientationContext> | null | undefined
  if (!raw || typeof raw !== 'object' || typeof raw.revision !== 'number') return null
  return {
    revision: raw.revision,
    previous_revision: typeof raw.previous_revision === 'number' ? raw.previous_revision : null,
    changes: Array.isArray(raw.changes) ? raw.changes.filter((c) => c && typeof c.says === 'string') : [],
    omitted: Array.isArray(raw.omitted) ? raw.omitted.filter((o) => typeof o === 'string') : [],
  }
}

/// "guide \"Workspace\" (`guide-workspace`) cut short, 8,000 chars — call
/// `doc_read` for `guide-workspace`".
export function missingLine(m: MissingContext): string {
  const scope = m.partial ? 'cut short' : 'not included'
  const size = m.chars > 0 ? `, ${formatTokens(m.chars)} chars` : ''
  return `${m.what} ${scope}${size}${m.fetch ? ` — ${m.fetch}` : ''}`
}
