// Turns the flat event log into what the session screen renders: consecutive
// tool calls fold into one group, open while any of them lacks a result, and a
// permission request breaks the group so it sits at its true position.

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
