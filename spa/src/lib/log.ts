// Turns the flat event log into what the session screen renders: consecutive
// tool calls fold into one group, and a permission request breaks the group so
// it sits at its true position. A group stays folded while a call in it runs:
// the running call is shown under the fold, so a run does not spring open and
// shut again with every call. Records
// the log shows nobody (a call the policy allowed, usage, a plan update) never
// break a run: every Claude Code call now carries a policy record, and letting
// those through left no two calls side by side.

import { exhaustionNote } from './exhaustion'
import { formatDuration, formatTokens } from './format'
import { callKind } from './transcript'
import type { Event, SessionExhaustion } from './types'

export interface ToolEntry {
  call: Event
  result?: Event
  progress?: string
  /// The end of what a running call has printed so far, when the stream
  /// carries it.
  tail?: string
  /// What the call did, from its policy record: the harness labels every call
  /// `other`, and the policy has already worked out read, edit or execute.
  kind?: string
}

/// A record that says only that the policy let a call through. One the
/// policy would have asked about or refused (a harness that ran it without
/// asking) is not quiet: that is for the operator to see.
function quietAllow(e: Event): boolean {
  if (e.kind !== 'policy_allowed') return false
  const verdict = e.payload.policy
  return verdict === undefined || verdict === null || verdict === 'allow'
}

/// Kinds the log renders nothing for; they must not end a run either.
export const SILENT = new Set(['usage', 'plan'])

export interface LogEntry {
  kind: 'leaf' | 'tools'
  event?: Event
  tools?: ToolEntry[]
}

export function groupLog(
  events: Event[],
  progress: Map<string, string> = new Map(),
  tails: Map<string, string> = new Map(),
): LogEntry[] {
  const out: LogEntry[] = []
  const openCalls = new Map<string, ToolEntry>()

  for (const e of events) {
    if (e.kind === 'tool_call') {
      const entry: ToolEntry = { call: e, progress: progress.get(e.ref_id ?? ''), tail: tails.get(e.ref_id ?? '') }
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
    if (quietAllow(e)) {
      // Lend its kind to the call it decided: by id when the record names
      // one, else the call just made, which the record always follows.
      const id = typeof e.payload.tool_call_id === 'string' ? e.payload.tool_call_id : undefined
      const last = out[out.length - 1]
      const entry = id ? openCalls.get(id) : last?.kind === 'tools' ? last.tools![last.tools!.length - 1] : undefined
      const kind = e.payload.kind
      if (entry && !entry.kind && typeof kind === 'string' && kind) entry.kind = kind
      continue
    }
    if (SILENT.has(e.kind)) continue
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

/// Where the harness is running, appended to the start line. Said only when
/// there is something to say: the repository's image, or why a repository
/// that has one is not being used. A session in the plain harness image, with
/// no repository image to miss, adds nothing.
export function sessionImage(payload: Record<string, unknown>): string {
  if (typeof payload.image === 'string' && payload.image) return " · in the repository's image"
  if (typeof payload.image_note === 'string' && payload.image_note) {
    return ` · in the harness image, not the repository's: ${payload.image_note}`
  }
  return ''
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

/// A policy record worth a line of its own: "refused by policy · Bash: rm -rf
/// / · reason", or "ran without asking · the policy would have asked · …" for
/// a call the harness let through on its own rules that the node would not
/// have. A plain allow never gets here; `groupLog` folds it into its call.
export function policyLine(e: Event): string {
  const p = e.payload
  const text = (v: unknown) => (typeof v === 'string' && v.trim() ? v.trim() : '')
  const what = text(p.title) || text(p.command) || text(p.action) || 'a call'
  const why = text(p.reason)
  if (e.kind === 'policy_denied') return ['refused by policy', what, why].filter(Boolean).join(' · ')
  const would = p.policy === 'deny' ? 'refused it' : 'asked'
  const rule = text(p.rule)
  return ['ran without asking', `the policy would have ${would}${rule ? ` (${rule})` : ''}`, what]
    .filter(Boolean)
    .join(' · ')
}

/// A run with a call still waiting for its result: it reads the clock, and
/// its running calls show under the fold.
export function groupRunning(tools: ToolEntry[]): boolean {
  return tools.some((t) => !t.result)
}

/// "Read 2 files, ran 1 shell command" — the folded summary line.
export function groupSummary(tools: ToolEntry[]): string {
  const counts = new Map<string, number>()
  for (const t of tools) {
    const kind = callKind(t)
    counts.set(kind, (counts.get(kind) ?? 0) + 1)
  }
  const labels: Record<string, [string, string]> = {
    read: ['read %d file', 'read %d files'],
    edit: ['edited %d file', 'edited %d files'],
    execute: ['ran %d shell command', 'ran %d shell commands'],
    search: ['ran %d search', 'ran %d searches'],
    think: ['updated the plan', 'updated the plan'],
    fetch: ['fetched %d page', 'fetched %d pages'],
    other: ['called %d tool', 'called %d tools'],
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

/** One line of the log for an event with no markup of its own. */
export interface EventLine {
  text: string
  /** `sys` is the node talking to itself; the rest are `mark` lines in that colour. */
  tone: 'sys' | 'mark' | 'wait' | 'crit' | 'ok'
  /** A link after the text: a forge URL, or a route in this app. */
  href?: string
  link?: string
}

function str(v: unknown): string {
  return typeof v === 'string' ? v.trim() : ''
}

function num(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}

function line(tone: EventLine['tone'], parts: unknown[], href?: string, link?: string): EventLine {
  const text = parts.filter((p) => typeof p === 'string' && p).join(' · ')
  return href ? { text, tone, href, link } : { text, tone }
}

/// A pause or resume reason, unless it is the node's stand-in for none.
function ownReason(v: unknown): string {
  const reason = str(v)
  return /^operator (paused|resumed) the session$/.test(reason) ? '' : reason
}

/// A forge URL without its scheme and host: "cosmic-example/orbit/pull/418".
function forgePath(url: string): string {
  try {
    const u = new URL(url)
    return u.pathname.replace(/^\/+|\/+$/g, '') || u.host
  } catch {
    return url
  }
}

/// The line for every event kind Log.svelte gives no markup of its own, or
/// null for a kind this does not know. Only what the node recorded: a field
/// it left out is left out of the line, never filled with a guess.
export function eventLine(e: Event): EventLine | null {
  const p = e.payload
  switch (e.kind) {
    case 'session_paused': {
      const source = str(p.source)
      // The exhaustion line just before says what the provider did and what
      // happens next; repeating its sentence here says it twice.
      if (source === 'exhausted') return line('wait', ['paused', 'its provider is exhausted'])
      const who = source === 'watchdog' ? 'paused by the node' : 'paused by the operator'
      return line('wait', [who, ownReason(p.reason)])
    }
    case 'session_resumed': {
      const who = str(p.source) === 'wake' ? 'resumed by the node' : 'resumed by the operator'
      return line('mark', [who, ownReason(p.reason)])
    }
    case 'workspace_prepared': {
      // The repository's own `prepare`, run on the workspace before the
      // harness started; what it cost the start is said either way.
      const took = num(p.ms) !== null ? formatDuration(num(p.ms)!) : ''
      const commands = Array.isArray(p.commands) ? p.commands.filter((c): c is string => typeof c === 'string') : []
      switch (str(p.outcome)) {
        case 'prepared':
          return line('mark', ['workspace prepared', commands.join(' · '), took])
        case 'failed':
          return line('crit', ['workspace not prepared', 'preparation failed', 'the session started without it', took])
        case 'timed_out': {
          const limit = num(p.timeout_secs)
          return line('crit', ['workspace not prepared', limit !== null ? `timed out after ${formatDuration(limit * 1000)}` : 'timed out', 'the session started without it'])
        }
        case 'cancelled':
          return line('mark', ['workspace preparation stopped', str(p.reason)])
        default:
          return line('sys', ['workspace not prepared', str(p.reason)])
      }
    }
    case 'published': {
      const url = str(p.url)
      return line('ok', ['published', url ? forgePath(url) : ''], url || undefined, url ? 'open' : undefined)
    }
    case 'session_suspended': {
      const idle = num(p.idle_ms)
      return line('mark', ['suspended', idle !== null ? `idle ${formatDuration(idle)} after publishing` : 'idle after publishing'])
    }
    case 'approval_settled': {
      const state = str(p.state) || 'settled'
      const tone = state === 'succeeded' ? 'ok' : state === 'failed' ? 'crit' : state === 'uncertain' ? 'wait' : 'mark'
      const what = state === 'uncertain' ? 'outcome unknown' : state
      return line(tone, [`approved call ${what}`, str(p.tool), state === 'succeeded' ? '' : str(p.reason)])
    }
    case 'candidate_verified': {
      const sha = str(p.head_sha).slice(0, 12)
      return line('ok', ['checks passed', sha ? `at ${sha}` : '', p.reused === true ? 'evidence reused' : ''])
    }
    case 'review_decision': {
      const decision = str(p.decision)
      const said: Record<string, [string, EventLine['tone']]> = {
        approved: ['review approved', 'ok'],
        rejected: ['review rejected', 'crit'],
        revise: ['review sent back with notes', 'wait'],
      }
      const [text, tone] = said[decision] ?? [`review ${decision || 'decided'}`, 'mark']
      const waited = num(p.waiting_ms)
      return line(tone, [text, 'by the operator', waited !== null ? `after ${formatDuration(waited)} waiting` : ''])
    }
    case 'provider_exhausted': {
      const note = exhaustionNote(p as unknown as SessionExhaustion)
      const provider = str(p.provider) || 'its provider'
      return line('wait', [note?.title ?? `${provider} is exhausted`, str(p.reason), note?.detail ?? str(p.note)])
    }
    case 'exhaustion_boundary': {
      const carried = str(p.outcome) === 'falling_back'
      return line('sys', ['the cut-off turn has settled', carried ? 'the fallback carries on from here' : 'the work resumes from here'])
    }
    case 'exhaustion_wake': {
      const outcome = str(p.outcome)
      const note = str(p.note)
      switch (outcome) {
        case 'resumed':
          return line('mark', ["the provider's limit reset", 'resumed', note])
        case 'continued': {
          const next = str(p.continued_by)
          return line('mark', ['continued in a new session', str(p.model) ? `on ${str(p.model)}` : '', note], next ? `/sessions/${next}` : undefined, next ? next.slice(0, 8) : undefined)
        }
        case 'held':
          return line('wait', ['held for you', note])
        case 'operator':
          return line('mark', ['taken over by the operator', note])
        case 'abandoned':
          return line('sys', ['not woken', note])
        default:
          return line('mark', ['exhaustion', outcome, note])
      }
    }
    case 'uncertain': {
      const cleared = str(p.cleared)
      if (cleared) return line('sys', ['outcome settled', cleared])
      return line('wait', ['outcome unknown', str(p.reason), 'new prompts refused until it settles'])
    }
    case 'child_session':
      return line('wait', ['the harness started a session of its own', str(p.child), 'recorded, not driven'])
    case 'pty_opened': {
      if (str(p.phase) === 'attach') return line('wait', ['terminal attached', str(p.pty_id)])
      const args = Array.isArray(p.args) ? p.args.filter((a) => typeof a === 'string') : []
      const command = [str(p.command), ...args].filter(Boolean).join(' ')
      return line('wait', ['terminal opened', command, str(p.cwd) ? `in ${str(p.cwd)}` : '', 'what is typed in it is not recorded'])
    }
    case 'pty_closed': {
      const duration = num(p.duration_ms)
      const what = str(p.phase) === 'detached' ? 'terminal detached' : 'terminal closed'
      return line('sys', [what, duration !== null ? `after ${formatDuration(duration)}` : '', str(p.reason)])
    }
    case 'service': {
      const state = str(p.state) || 'changed'
      const tone = state === 'failed' ? 'crit' : state === 'ready' ? 'ok' : 'mark'
      return line(tone, [`service ${str(p.name) || 'unnamed'} ${state}`, str(p.detail)])
    }
    case 'abandoned':
      return line('mark', ['abandoned', str(p.summary) || str(p.reason)])
    default:
      return null
  }
}

/// A kind nobody taught the log: its name in words and its short fields,
/// "some kind · name postgres · state ready", never a bare identifier. A
/// stand-in until it gets a line of its own; the test over the node's kinds
/// is what notices one is missing.
export function fallbackLine(e: Event): EventLine {
  const fields = Object.entries(e.payload)
    .filter(([, v]) => (typeof v === 'string' && v.trim() && v.length <= 80) || typeof v === 'number' || typeof v === 'boolean')
    .slice(0, 3)
    .map(([k, v]) => `${k.replace(/_/g, ' ')} ${String(v).trim()}`)
  return line('sys', [e.kind.replace(/_/g, ' '), ...fields])
}
