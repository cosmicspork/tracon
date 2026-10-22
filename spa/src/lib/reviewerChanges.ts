// The AI reviewer's request for changes, sent on as the operator's own. The
// operator opens it in the Request changes composer, edits it or drops
// findings, and sends it; nothing reaches the agent without that click. The
// text is marked as from the AI review so the agent and the history can tell
// it from a request the operator wrote.
//
// In the composer the reviewer's text sits between two tag lines naming the
// verdict it came from, so the block is found again from the text alone, in
// a draft restored after a reload as much as on this screen. The tags are
// taken out before it is sent.

import type { AiFinding } from './types'

/** The line that marks a request for changes as the AI reviewer's, sent by the operator. */
export const FROM_AI_REVIEW = 'From the AI review, sent by the operator.'

/** The tag line that opens a verdict's block in the composer. */
export function openTag(at: number): string {
  return `<!-- ai-review ${at} -->`
}

/** The tag line that closes it. */
export const CLOSE_TAG = '<!-- /ai-review -->'

const OPEN = /^<!-- ai-review (\S+) -->$/

/** What a field of a finding holds as text, whatever the reviewer sent. */
function text(value: unknown): string {
  if (typeof value === 'string') return value
  if (value === null || value === undefined) return ''
  return String(value)
}

/**
 * One finding as one item of the request: `- [severity] path:line — note`.
 * A finding with no path or line says only what it has; a note of several
 * lines keeps them, indented under the item. A field that is not what its
 * name says is read as text rather than stopping the composer opening.
 */
export function findingLine(f: AiFinding): string {
  const path = text(f.path)
  const line = typeof f.line === 'number' && f.line > 0 ? f.line : null
  const where = path ? (line ? `${path}:${line}` : path) : line ? `line ${line}` : ''
  const [first, ...rest] = text(f.note).trim().split('\n')
  const head = `- [${text(f.severity) || 'should'}] ${where ? `${where} — ` : ''}${first.trim()}`
  return [head, ...rest.map((l) => (l.trim() ? `  ${l.trimEnd()}` : ''))].join('\n')
}

/** A verdict's summary and findings, and when it was given: what names its block. */
export interface AiRequest {
  summary: string
  findings: AiFinding[]
  at: number
}

/** The composer's text for a reviewer's verdict: its summary and each finding one item, tagged with the verdict. */
export function requestText(request: AiRequest): string {
  const items = request.findings.map(findingLine)
  const body = [FROM_AI_REVIEW, text(request.summary).trim(), items.join('\n')].filter((part) => part !== '')
  return [openTag(request.at), body.join('\n\n'), CLOSE_TAG].join('\n')
}

/** Where a verdict's block sits in the text, by its tag lines, and which verdict it is. */
function aiBlock(lines: string[]): { start: number; end: number; at: string } | null {
  const start = lines.findIndex((line) => OPEN.test(line))
  if (start < 0) return null
  const end = lines.indexOf(CLOSE_TAG, start + 1)
  if (end < 0) return null
  return { start, end, at: OPEN.exec(lines[start])![1] }
}

/**
 * Where a block of whole lines sits in `lines`, or -1. A finding's item
 * ends where its indented continuation does, so one item is never found
 * inside a longer one that starts the same way.
 */
function blockAt(lines: string[], block: string[]): number {
  for (let i = 0; i + block.length <= lines.length; i++) {
    if (!block.every((line, j) => lines[i + j] === line)) continue
    const next = lines[i + block.length]
    if (next === undefined || !next.startsWith('  ')) return i
  }
  return -1
}

function has(text: string, block: string): boolean {
  return blockAt(text.split('\n'), block.split('\n')) >= 0
}

function closedUp(lines: string[]): string {
  return lines.join('\n').replace(/\n{3,}/g, '\n\n').trim()
}

/** The text with a block of whole lines taken out, and the blank lines it leaves closed up. */
function without(text: string, block: string): string {
  const lines = text.split('\n')
  const parts = block.split('\n')
  const at = blockAt(lines, parts)
  if (at < 0) return text
  lines.splice(at, parts.length)
  return closedUp(lines)
}

/**
 * What the composer holds when it is opened on a reviewer's verdict: what
 * was there, with the verdict's summary and findings in front of it, and
 * which findings are ticked. Opened again on the same verdict (Back, then
 * Request these changes… again, or a draft restored after a reload), it is
 * not added a second time. Opened on another verdict, that verdict's block
 * is replaced by this one's. A finding is ticked while its item is in the
 * text.
 */
export function openAiRequest(reason: string, request: AiRequest): { text: string; ticked: boolean[] } {
  const lines = reason.split('\n')
  const block = aiBlock(lines)
  let result: string
  if (block && block.at === String(request.at)) {
    result = reason
  } else if (block) {
    lines.splice(block.start, block.end - block.start + 1, ...requestText(request).split('\n'))
    result = closedUp(lines)
  } else {
    result = reason.trim() ? `${requestText(request)}\n\n${reason.trim()}` : requestText(request)
  }
  return { text: result, ticked: request.findings.map((f) => has(result, findingLine(f))) }
}

/**
 * The text with one finding ticked on or off. Unticking takes its item out
 * wherever it stands, matched as whole lines; ticking puts it back in its
 * place among the items still there, or, when none are, at the end of the
 * verdict's block. Whatever else the operator wrote is left as it is.
 */
export function withFinding(text: string, finding: AiFinding, on: boolean, findings: AiFinding[]): string {
  const item = findingLine(finding)
  if (!on) return without(text, item)
  if (has(text, item)) return text
  const items = findings.map(findingLine)
  const index = items.indexOf(item)
  const lines = text.split('\n')
  const before = items
    .slice(0, index)
    .filter((other) => has(text, other))
    .at(-1)
  if (before !== undefined) {
    const at = blockAt(lines, before.split('\n')) + before.split('\n').length
    lines.splice(at, 0, ...item.split('\n'))
    return lines.join('\n')
  }
  const after = items.slice(index + 1).find((other) => has(text, other))
  if (after !== undefined) {
    lines.splice(blockAt(lines, after.split('\n')), 0, ...item.split('\n'))
    return lines.join('\n')
  }
  const block = aiBlock(lines)
  if (block) {
    lines.splice(block.end, 0, '', ...item.split('\n'))
    return closedUp(lines)
  }
  return `${text.trimEnd()}\n\n${item}`
}

/** Whether the text holds a verdict's block, as a draft restored after a reload may. */
export function hasAiBlock(text: string): boolean {
  return aiBlock(text.split('\n')) !== null
}

function untagged(text: string): string {
  return closedUp(text.split('\n').filter((line) => !OPEN.test(line.trim()) && line.trim() !== CLOSE_TAG))
}

/** What is sent: the operator's text without the composer's tags, still marked as from the AI review. */
export function markedFromAi(text: string): string {
  const sent = untagged(text)
  return sent.includes(FROM_AI_REVIEW) ? sent : `${FROM_AI_REVIEW}\n\n${sent}`
}

/**
 * The reason sent with a request for changes or a rejection, whichever
 * composer it was written in: marked as from the AI review while a
 * verdict's block is in it, and never carrying the composer's tags. Text
 * the operator wrote in place of the block is their own.
 */
export function sentReason(text: string): string {
  return hasAiBlock(text) ? markedFromAi(text) : untagged(text)
}
