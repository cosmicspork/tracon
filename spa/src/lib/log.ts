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
