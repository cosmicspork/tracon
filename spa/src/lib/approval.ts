// A held call as the operator reads and edits it on its own page. Which
// arguments are prose, how that prose is written, and which arguments name
// the target all come from the node, so nothing here knows a tool by name.
import { diffArguments } from './text-diff'
import { formArguments, formFields, validate, type Field, type FieldValue, type JsonSchema } from './schema-form'

export type ApprovalState =
  | 'pending'
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'uncertain'
  | 'rejected'
  | 'changes_requested'
  | 'expired'

export interface ApprovalRow {
  id: string
  channel: string
  session_id: string | null
  node_id: string
  lane: string | null
  tool: string
  arguments: string
  request_key: string
  title: string
  state: ApprovalState
  answer_option_id: string | null
  edited_arguments: string | null
  result: string | null
  /** Why it was refused, expired, or failed. */
  reason: string | null
  /** The operator's notes on a request for changes, or a remark with an allow. */
  operator_note: string | null
  claimed_ms: number | null
  created_ms: number
  decided_ms: number | null
  finished_ms: number | null
  expires_ms: number
}

export interface ApprovalDetails {
  approval: ApprovalRow
  arguments: unknown
  edited_arguments: unknown
  result: unknown
  input_schema: JsonSchema | null
  /** `jira_wiki` or `markdown`. */
  format: string
  prose_fields: string[]
  locked_fields: string[]
  lane: string | null
  /** When an operator last had it open before this load, or null. */
  claimed_before_ms: number | null
  options: { option_id: string; name: string; kind: string }[]
}

/** One problem with the answer, as the node's 422 or `validate` names it. */
export interface AnswerProblem {
  field?: string
  path?: string
  message: string
}

export function asArguments(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : {}
}

function jsonType(value: unknown): string | undefined {
  if (typeof value === 'string') return 'string'
  if (typeof value === 'boolean') return 'boolean'
  if (Number.isInteger(value)) return 'integer'
  if (typeof value === 'number') return 'number'
  if (Array.isArray(value)) return value.every((v) => typeof v === 'string') ? 'array' : undefined
  return undefined
}

/** The schema to build the form from, or one read off the arguments when the node knows none. */
export function schemaFor(schema: JsonSchema | null, args: Record<string, unknown>): JsonSchema {
  if (schema?.properties) return schema
  return {
    type: 'object',
    properties: Object.fromEntries(
      Object.entries(args).map(([k, v]) => {
        const type = jsonType(v)
        return [k, type === 'array' ? { type, items: { type: 'string' } } : type ? { type } : {}]
      }),
    ),
  }
}

/**
 * The form's fields, with the node's word on which are prose and which are
 * locked over the guesses schema-form makes on its own. Locked fields come
 * first, so what the call acts on reads as one block above what can change.
 */
export function approvalFields(
  schema: JsonSchema,
  args: Record<string, unknown>,
  prose: readonly string[],
  locked: readonly string[],
): Field[] {
  const all = formFields(schema, args).map((f) => {
    const multiline = typeof args[f.key] === 'string' && (args[f.key] as string).includes('\n')
    const kind =
      prose.includes(f.key) && (f.kind === 'text' || f.kind === 'prose')
        ? 'prose'
        : f.kind === 'prose' && !multiline
          ? 'text'
          : f.kind
    return { ...f, kind, locked: locked.includes(f.key) }
  })
  return [...all.filter((f) => f.locked), ...all.filter((f) => !f.locked)]
}

/**
 * Arguments that only say how long the asking call blocks for the answer.
 * They are the agent's own tuning, not part of what is being decided.
 */
export const CALLER_ONLY: readonly string[] = ['wait_secs']

/**
 * Whether allowing this approval grants access for a scope rather than
 * running the call: the answers offer a scope beyond once (a host for the
 * session or the repository). The node reads no edit with such an answer,
 * so the request is shown as asked rather than as a form.
 */
export function grantsAccess(options: readonly { kind: string }[]): boolean {
  return options.some((o) => o.kind === 'allow_session' || o.kind === 'allow_repo')
}

/** A tool's result as rows to show, or null when it is not an object. */
export function resultRows(result: unknown): { key: string; value: string; href: string | null }[] | null {
  if (!result || typeof result !== 'object' || Array.isArray(result)) return null
  return Object.entries(result as Record<string, unknown>)
    .filter(([, v]) => v !== null && v !== undefined)
    .map(([key, v]) => {
      const value = typeof v === 'string' ? v : JSON.stringify(v)
      return { key, value, href: typeof v === 'string' && /^https?:\/\//.test(v) ? v : null }
    })
}

/** The arguments the form describes. A locked field is always the agent's own. */
export function approvalArguments(
  fields: Field[],
  values: Record<string, FieldValue>,
  original: Record<string, unknown>,
): Record<string, unknown> {
  const out = formArguments(
    fields.filter((f) => !f.locked),
    values,
    original,
  )
  for (const f of fields) {
    if (!f.locked) continue
    if (f.key in original) out[f.key] = original[f.key]
    else delete out[f.key]
  }
  return out
}

/** The edit to send with Allow: the whole rewrite when anything changed, else nothing. */
export function changedArguments(
  original: Record<string, unknown>,
  edited: Record<string, unknown>,
): Record<string, unknown> | undefined {
  return diffArguments(original, edited).length ? edited : undefined
}

/**
 * What is wrong with an edit before it is sent. Like the node, it holds only
 * the fields the operator changed against the schema: what the agent sent and
 * the operator left alone is the call as it was asked.
 */
export function editProblems(
  schema: JsonSchema,
  original: Record<string, unknown>,
  edited: Record<string, unknown>,
): AnswerProblem[] {
  const touched = (path: string) => {
    const top = topKey(path)
    return top === '' || JSON.stringify(original[top]) !== JSON.stringify(edited[top])
  }
  return validate(schema, edited).filter((e) => touched(e.path))
}

function topKey(path: string): string {
  return path.split(/[.[]/)[0] ?? ''
}

/**
 * Problems grouped by the top-level field they belong to, so each shows under
 * its own input. `labels[1]` goes to `labels`. A problem that names no field,
 * or one that is not in `keys`, goes under ''.
 */
export function problemsByField(problems: AnswerProblem[], keys: readonly string[]): Record<string, string[]> {
  const out: Record<string, string[]> = {}
  for (const p of problems) {
    const path = p.field ?? p.path ?? ''
    const top = topKey(path)
    const key = keys.includes(top) ? top : ''
    ;(out[key] ??= []).push(path && path !== key ? `${path}: ${p.message}` : p.message)
  }
  return out
}

const SETTLED: Record<string, string> = {
  running: 'Allowed · running now',
  succeeded: 'Allowed · it ran',
  failed: 'Allowed · it failed',
  uncertain: 'Allowed · the outcome could not be confirmed',
  rejected: 'Rejected · nothing ran',
  changes_requested: 'Changes requested · nothing ran',
  expired: 'Expired unanswered · nothing ran',
}

/** What became of a decided approval, or null while it waits. */
export function outcome(state: string): string | null {
  return state === 'pending' ? null : (SETTLED[state] ?? state)
}
