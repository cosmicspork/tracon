// A run of tool calls as a transcript to scan: one line per call with what it
// touched (a path, a command, a URL, read out of the call's `raw_input`), how
// it ended and how long it took. Consecutive reads share one head, a failed
// call carries its first error line, and a running one the end of its output.
// `groupLog` decides where a run starts and ends; this only says what is in it.

import type { ToolEntry } from './log'

export type CallState = 'running' | 'done' | 'failed'

export interface CallRow {
  entry: ToolEntry
  kind: string
  /** The line as read: "Read src/lib/log.ts", "Bash just test". */
  text: string
  state: CallState
  /** How long it took, or has been running; null when the log cannot say. */
  ms: number | null
  /** What the call returned, as text. Empty when nothing was recorded. */
  output: string
  truncated: boolean
  /** A failed call's first error line, shown without opening anything. */
  error: string
  /** The last lines a running call has printed so far. */
  tail: string
}

export type TranscriptRow =
  | { kind: 'call'; call: CallRow }
  | { kind: 'reads'; calls: CallRow[]; text: string; state: CallState; ms: number | null }

// What a call did by the name of its tool, for a call no policy record
// labelled: every harness calls its tools `other`.
const BY_NAME: Record<string, string> = {
  read: 'read',
  view: 'read',
  notebookread: 'read',
  edit: 'edit',
  multiedit: 'edit',
  write: 'edit',
  patch: 'edit',
  notebookedit: 'edit',
  bash: 'execute',
  shell: 'execute',
  exec: 'execute',
  grep: 'search',
  glob: 'search',
  ls: 'search',
  list: 'search',
  webfetch: 'fetch',
  websearch: 'fetch',
  fetch: 'fetch',
  todowrite: 'think',
}

function str(v: unknown): string {
  return typeof v === 'string' ? v.trim() : ''
}

/// read, edit, execute, search, fetch, think or other: the policy's word
/// when it gave one, else the harness's, else the tool's name.
export function callKind(t: ToolEntry): string {
  const given = [t.kind, str(t.call.payload.kind)].find((k) => k && k !== 'other')
  if (given) return given
  const name = str(t.call.payload.title).split(/[\s:]/)[0].toLowerCase()
  return BY_NAME[name] ?? 'other'
}

/// A path inside the session's worktree, from the worktree: the absolute
/// prefix is the same on every line and says nothing.
export function relativePath(path: string, root?: string | null): string {
  const base = root?.replace(/\/+$/, '')
  return base && path.startsWith(`${base}/`) ? path.slice(base.length + 1) : path
}

/// What a call acted on, out of its `raw_input`: the command it ran, the file
/// it read or wrote, the pattern it searched for, the page it fetched. Only
/// the first line of a command: the rest is in its output.
export function callTarget(rawInput: unknown, root?: string | null): string {
  if (!rawInput || typeof rawInput !== 'object' || Array.isArray(rawInput)) return ''
  const input = rawInput as Record<string, unknown>
  const command = Array.isArray(input.command) ? input.command.filter((c) => typeof c === 'string').join(' ') : str(input.command)
  if (command) {
    const [first, ...rest] = command.split('\n')
    return rest.some((l) => l.trim()) ? `${first.trim()} …` : first.trim()
  }
  const path = str(input.file_path) || str(input.filePath) || str(input.notebook_path) || str(input.path)
  const pattern = str(input.pattern)
  if (pattern) return path ? `"${pattern}" in ${relativePath(path, root)}` : `"${pattern}"`
  if (path) return relativePath(path, root)
  return str(input.url) || str(input.query) || str(input.description)
}

/// The tool's name and what it acted on, once: a title that already names
/// its target ("Read node/src/x.rs") is left as it is.
export function callText(t: ToolEntry, root?: string | null): string {
  const title = str(t.call.payload.title) || t.call.ref_id || 'a tool'
  const target = callTarget(t.call.payload.raw_input, root)
  return target && !title.includes(target) ? `${title} ${target}` : title
}

export function callState(t: ToolEntry): CallState {
  const status = str(t.result?.payload.status)
  if (!t.result) return 'running'
  return status === 'failed' || status === 'error' ? 'failed' : 'done'
}

/// The node keeps a result's output as JSON, cut at a cap: whole, it parses
/// back to the text the tool printed; cut, it is a string literal with no
/// end, read here by hand so the part that was kept still reads as text.
export function decodeOutput(raw: unknown): string {
  if (typeof raw !== 'string' || !raw) return ''
  try {
    const v = JSON.parse(raw) as unknown
    if (typeof v === 'string') return v
    if (v === null) return ''
    return JSON.stringify(v, null, 2)
  } catch {
    if (!raw.startsWith('"')) return raw
    return raw
      .slice(1)
      .replace(/\\u([0-9a-fA-F]{4})/g, (_, h: string) => String.fromCharCode(parseInt(h, 16)))
      .replace(/\\(.)/g, (_, c: string) => ({ n: '\n', t: '\t', r: '' })[c] ?? c)
  }
}

const ERROR = /\b(error|errors|failed|failure|fatal|panic(ked)?|exception|denied|refused|not found|no such|cannot|can't|unable)\b/i

function clip(line: string, max = 160): string {
  const t = line.trim()
  return t.length > max ? `${t.slice(0, max - 1)}…` : t
}

/// The line that says why a call failed: the first that reads like an error,
/// else the first it printed at all.
export function firstErrorLine(output: string): string {
  const lines = output.split('\n').filter((l) => l.trim())
  return clip(lines.find((l) => ERROR.test(l)) ?? lines[0] ?? '')
}

/// The last lines of what a call has printed, for a call still running.
export function outputTail(output: string, count = 2): string {
  return output
    .split('\n')
    .filter((l) => l.trim())
    .slice(-count)
    .map((l) => clip(l))
    .join('\n')
}

/// "0.4s", "12s", "3m 20s", "1h 5m": precise enough to tell a slow call from
/// a quick one, which the log's ages ("2m") are not.
export function formatSpan(ms: number): string {
  const v = Math.max(0, ms)
  if (v < 10_000) return `${(v / 1000).toFixed(1)}s`
  const s = Math.round(v / 1000)
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return s % 60 ? `${m}m ${s % 60}s` : `${m}m`
  const h = Math.floor(m / 60)
  return m % 60 ? `${h}h ${m % 60}m` : `${h}h`
}

function callMs(t: ToolEntry, now: number): number | null {
  const end = t.result ? t.result.at_ms : now
  return end > 0 && t.call.at_ms > 0 ? Math.max(0, end - t.call.at_ms) : null
}

export function callRow(t: ToolEntry, root?: string | null, now = 0): CallRow {
  const state = callState(t)
  const output = decodeOutput(t.result?.payload.output)
  return {
    entry: t,
    kind: callKind(t),
    text: callText(t, root),
    state,
    ms: callMs(t, now),
    output,
    truncated: t.result?.payload.truncated === true,
    error: state === 'failed' ? firstErrorLine(output) : '',
    tail: state === 'running' ? outputTail(decodeOutput(t.tail)) : '',
  }
}

function worst(calls: CallRow[]): CallState {
  if (calls.some((c) => c.state === 'running')) return 'running'
  return calls.some((c) => c.state === 'failed') ? 'failed' : 'done'
}

function fileName(text: string): string {
  const last = text.split(/\s+/).pop() ?? text
  return last.split('/').pop() || last
}

/// The run's lines, consecutive reads under one head: "Read 3 files ·
/// model.rs, metrics.rs, log.ts". Pass `now` while the run is open, so a
/// running call says how long it has been going.
export function transcript(tools: ToolEntry[], root?: string | null, now = 0): TranscriptRow[] {
  const rows: TranscriptRow[] = []
  let reads: CallRow[] = []
  const flush = () => {
    if (reads.length === 1) rows.push({ kind: 'call', call: reads[0] })
    if (reads.length > 1) {
      const known = reads.every((c) => c.ms !== null)
      const start = Math.min(...reads.map((c) => c.entry.call.at_ms))
      const end = Math.max(...reads.map((c) => c.entry.call.at_ms + (c.ms ?? 0)))
      rows.push({
        kind: 'reads',
        calls: reads,
        text: `Read ${reads.length} files · ${reads.map((c) => fileName(c.text)).join(', ')}`,
        state: worst(reads),
        ms: known ? end - start : null,
      })
    }
    reads = []
  }
  for (const t of tools) {
    const row = callRow(t, root, now)
    if (row.kind === 'read') {
      reads.push(row)
      continue
    }
    flush()
    rows.push({ kind: 'call', call: row })
  }
  flush()
  return rows
}

/// How long a run took, first call to last result; for a run still going,
/// to `now`.
export function runMs(tools: ToolEntry[], now = 0): number | null {
  if (!tools.length) return null
  const start = Math.min(...tools.map((t) => t.call.at_ms))
  const open = tools.some((t) => !t.result)
  const end = open ? now : Math.max(...tools.map((t) => t.result!.at_ms))
  return start > 0 && end > 0 ? Math.max(0, end - start) : null
}

/// The failed calls of a run, each with its first error line: what a folded
/// run shows under its summary, so a failure is never behind a click.
export function runFailures(tools: ToolEntry[], root?: string | null): { text: string; error: string }[] {
  return tools
    .filter((t) => callState(t) === 'failed')
    .map((t) => {
      const row = callRow(t, root)
      return { text: row.text, error: row.error }
    })
}
