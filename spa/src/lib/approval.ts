// A held call as the operator reads and edits it on its own page. Which
// arguments are prose, how that prose is written, and which arguments name
// the target all come from the node, so nothing here knows a tool by name.
import { diffArguments, diffLines, hunks, type Hunk } from './text-diff'
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
  /** What an answer names to say it was given for these arguments. */
  arguments_hash?: string
  edited_arguments: unknown
  result: unknown
  input_schema: JsonSchema | null
  /** `markdown`, or `source` (shown as text, never rendered). */
  format: string
  /**
   * The format the prose fields are written in when a plugin owns it
   * (`jira_wiki`): `rendered` holds them as Markdown, converted by that
   * plugin, and the arguments stay the source. Null otherwise.
   */
  plugin_format?: string | null
  /** The prose fields, by name, rendered as Markdown by the plugin that owns their format. */
  rendered?: Record<string, string>
  /** `rendered` for each item of the item list, in order. */
  rendered_items?: Record<string, string>[] | null
  /** Why the prose could not be rendered: the page shows the source. */
  render_error?: string
  prose_fields: string[]
  locked_fields: string[]
  /**
   * The argument that, when given, is a list of items each shown as its own
   * section, with the prose fields read inside each item; null for a tool
   * that takes no list.
   */
  item_list?: string | null
  lane: string | null
  /** When an operator last had it open before this load, or null. */
  claimed_before_ms: number | null
  options: { option_id: string; name: string; kind: string }[]
  /**
   * What the call would replace, as it stands now: the document a
   * `doc_write` lands on, or null when the slug is new. Absent for a tool
   * that replaces nothing.
   */
  document?: ReplacedDocument | null
  /**
   * The body a `doc_edit`'s edits make of the document as it stands, or
   * null when they no longer apply to it. Absent for any other tool.
   */
  proposed_body?: string | null
  /** The `if_hash` the agent sent no longer matches: Allow would conflict. */
  stale?: boolean
  /**
   * The memory a `memory_amend` rewrites, as it stands now, or null when it
   * is no longer one the call could amend. Absent for any other tool.
   */
  memory?: ReplacedMemory | null
  /**
   * What a plugin's tool would do, in the plugin's own words, asked when
   * the page opened. Absent for any other tool, and for a plugin that has
   * no previews: the generic card.
   */
  preview?: PluginPreview
  /**
   * For a plugin's tool: its build, what it says the tool does and reaches,
   * and what an approved run is lent. Null for any other tool.
   */
  plugin?: PluginContext | null
}

export interface PluginContext {
  name: string
  /** The build the call was asked on. */
  version: string
  /**
   * The build active now, when the plugin was updated or rolled back after
   * the call was asked: allowing it then runs nothing.
   */
  replaced_by?: string | null
  tool: string
  /** What the plugin says the tool does. It decides nothing. */
  hint: 'read' | 'write'
  consequential: boolean
  /**
   * For a consequential tool, what a `plugin_call` grant allowing exactly
   * this call names: it is asked unless one does. Null otherwise.
   */
  grant_target?: string | null
  /** Hosts it says it reaches: shown, not enforced. */
  egress: string[]
  credentials: PluginCredential[]
}

export interface PluginCredential {
  /** The credential as the manifest declares it. */
  name: string
  /** `approved`: lent only to a call from an approved card, as this one. */
  lend: 'always' | 'approved'
  /** The channel's bindings it matches: what the run is lent. */
  bindings: string[]
}

/**
 * The rows a plugin tool's card adds: which build, what it says of the tool
 * and where it reaches (its own word, never checked), and the credentials an
 * approved run is lent. Empty for any other tool.
 */
export function pluginRows(details: Pick<ApprovalDetails, 'plugin'>): { key: string; value: string }[] {
  const p = details.plugin
  if (!p) return []
  const lent = p.credentials.flatMap((c) =>
    c.bindings.map((b) => (c.lend === 'approved' ? `${b} (only because you approve this card)` : b)),
  )
  const replaced = p.replaced_by
    ? [
        {
          key: 'Build changed',
          value: `${p.name} ${p.replaced_by} is active now, so allowing this runs nothing: the agent has to ask again`,
        },
      ]
    : []
  return [
    { key: 'Plugin', value: `${p.name} ${p.version}` },
    ...replaced,
    { key: 'Says it', value: p.consequential ? `${p.hint}s, consequential` : `${p.hint}s` },
    { key: 'Reaches', value: p.egress.length > 0 ? p.egress.join(', ') : 'declares none' },
    { key: 'Lent on allow', value: lent.length > 0 ? lent.join(', ') : 'nothing' },
    ...(p.grant_target ? [{ key: 'Grant target', value: `plugin_call ${p.grant_target}` }] : []),
  ]
}

export type PluginPreview =
  | {
      status: 'shown'
      /** False: the plugin cannot show the effect; `body` says why and shows what it can. */
      previewable: boolean
      /** One line, for the preview's heading. */
      summary: string
      /** Markdown: the diff, table or statement. */
      body: string
      /**
       * The hash of the token the preview was taken against, which the
       * answer sends back so it applies only to this preview. Null when
       * there is none.
       */
      token_sha256: string | null
    }
  | {
      status: 'refused'
      /** The plugin's refusal: the call can still be decided. */
      error: string
    }

/** The hash an answer names to say which preview it was read with, if any. */
export function previewHash(details: Pick<ApprovalDetails, 'preview'>): string | undefined {
  const p = details.preview
  return p?.status === 'shown' && p.token_sha256 ? p.token_sha256 : undefined
}

/**
 * A prose field as Markdown: what the plugin owning its format rendered,
 * when it did and the text is still what was rendered; the text itself when
 * the node renders its format; null when only the source can be shown.
 */
export function renderedProse(
  details: Pick<ApprovalDetails, 'plugin_format' | 'rendered' | 'rendered_items' | 'arguments' | 'item_list'>,
  key: string,
  text: string,
  item?: number,
): string | null {
  if (!details.plugin_format) return text
  const asked = asArguments(details.arguments)
  const list = details.item_list
  const original =
    item === undefined
      ? asked[key]
      : list && Array.isArray(asked[list])
        ? asArguments((asked[list] as unknown[])[item])[key]
        : undefined
  const rendered = item === undefined ? details.rendered?.[key] : details.rendered_items?.[item]?.[key]
  return rendered !== undefined && original === text ? rendered : null
}

export interface ReplacedMemory {
  id: string
  kind: string
  scope: string
  scope_ref: string | null
  body: string
  state: string
  updated_ms: number
}

export interface ReplacedDocument {
  body: string
  hash: string
  /** `markdown` or `html`. */
  format: string
  title: string
  updated_ms: number
}

/** The argument whose text replaces the current value. */
export const REPLACING_FIELD = 'body'

export interface Replacement {
  key: string
  /** What the call replaces the text of, for the heading. */
  subject: 'document' | 'memory'
  /** How the heading names the change: new, or an edit of what. */
  label: string
  /** Nothing stands there yet, so the proposal is all added. */
  created: boolean
  title: string | null
  format: string | null
  hunks: Hunk[]
  added: number
  removed: number
}

/**
 * The format worth naming: the document's, or the change between it and the
 * format the call writes (`markdown` when it names none).
 */
function formatLabel(current: string | null, requested: unknown): string | null {
  const to = typeof requested === 'string' && requested !== '' ? requested : 'markdown'
  if (current === null) return to
  return current === to ? current : `${current} → ${to}`
}

/**
 * The proposal against what it replaces, as folded line hunks, or null when
 * the node sent nothing to compare with. `args` is the operator's draft, so
 * the diff follows their edit.
 */
export function replacement(
  details: Pick<ApprovalDetails, 'document' | 'memory' | 'proposed_body'>,
  args: Record<string, unknown>,
): Replacement | null {
  const body = args[REPLACING_FIELD]
  // A `doc_edit` carries edits, not a body: the node sends the body they
  // make, or null when they no longer apply, which changes nothing here.
  // That body wins over any `body` argument, which a doc_edit never uses and
  // would otherwise let the caller pick the diff the operator sees.
  const proposed = (before: string) =>
    details.proposed_body !== undefined ? (details.proposed_body ?? before) : typeof body === 'string' ? body : ''
  const diff = (before: string) => {
    const lines = diffLines(before, proposed(before))
    return {
      key: REPLACING_FIELD,
      hunks: hunks(lines),
      added: lines.filter((l) => l.kind === 'add').length,
      removed: lines.filter((l) => l.kind === 'del').length,
    }
  }
  // A memory no longer there has nothing to diff against: the page says so.
  if (details.memory) {
    const m = details.memory
    const title = `${m.kind} · ${m.scope}`
    return { ...diff(m.body), subject: 'memory', label: `edit · ${title}`, created: false, title, format: null }
  }
  if (details.document === undefined) return null
  const current = details.document
  return {
    ...diff(current?.body ?? ''),
    subject: 'document',
    label: current === null ? 'new document' : `edit · “${current.title}”`,
    created: current === null,
    title: current?.title ?? null,
    format: formatLabel(current?.format ?? null, args.format),
  }
}

/** One item of a list argument, as the page shows it. */
export interface ItemSection {
  index: number
  /** Every other field with a value, in the item schema's order, as text. */
  rows: { key: string; value: string }[]
  /** The prose fields with text, to render as documents. */
  prose: { key: string; text: string }[]
}

/**
 * The items of the list argument `list` in `args`, each as its own section,
 * or [] when the call does not carry one. The page shows the set as it
 * would run; the list itself is edited as JSON.
 */
export function itemSections(
  schema: JsonSchema,
  args: Record<string, unknown>,
  list: string | null | undefined,
  prose: readonly string[],
): ItemSection[] {
  const items = list ? args[list] : undefined
  if (!Array.isArray(items)) return []
  const order = Object.keys(schema.properties?.[list!]?.items?.properties ?? {})
  return items.map((item, index) => {
    const fields = asArguments(item)
    const keys = [...order.filter((k) => k in fields), ...Object.keys(fields).filter((k) => !order.includes(k))]
    const shown = (v: unknown) => v !== undefined && v !== null && v !== '' && !(Array.isArray(v) && !v.length)
    return {
      index,
      rows: keys
        .filter((k) => !prose.includes(k) && shown(fields[k]))
        .map((k) => {
          const v = fields[k]
          const value =
            typeof v === 'string'
              ? v
              : Array.isArray(v) && v.every((x) => typeof x === 'string')
                ? v.join(', ')
                : JSON.stringify(v)
          return { key: k, value }
        }),
      prose: prose
        .filter((k) => typeof fields[k] === 'string' && fields[k] !== '')
        .map((k) => ({ key: k, text: fields[k] as string })),
    }
  })
}

/** A held `memory_amend` whose memory is gone or no longer live: Allow would fail. */
export function amendsNothing(details: Pick<ApprovalDetails, 'memory'>): boolean {
  return details.memory === null
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

/** How the reason of a call failed as stale begins: what the card showed had moved, and nothing was sent. */
export const STALE_REASON = 'stale:'

/** What became of a decided approval, or null while it waits. */
export function outcome(state: string, reason?: string | null): string | null {
  if (state === 'failed' && reason?.startsWith(STALE_REASON)) return 'Allowed · failed · stale, nothing was sent'
  return state === 'pending' ? null : (SETTLED[state] ?? state)
}
