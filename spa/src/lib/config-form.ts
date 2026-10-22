// node.toml as forms, from the node's own description of every key
// (`GET /api/config` `keys`): which control a key gets, what it says about
// itself, whether this browser may change it, and the typed operations a
// save sends (`PATCH /api/config`). Pure, so the part where an edit could
// silently become a different edit is tested flat.

import type { ConfigKeySpec, ConfigOp, ConfigPatchResult, ConfigShape, NodeConfig } from './types'
import { ApiError } from './api'

/** The control a key is edited with. */
export type Control = 'bool' | 'number' | 'text' | 'lines' | 'select' | 'secret'

export interface Field {
  /** The key as the file spells it: `session.budget_tokens`, `service[1].port`. */
  key: string
  /** Its concrete table keys, indices left out: what errors are matched on. */
  segs: string[]
  spec: ConfigKeySpec
  control: Control
  label: string
  /** What a number counts, said beside it. */
  unit?: string
  choices?: string[]
  /** Leaving it empty takes the key out, which means "none" (or the node
      works it out), not a value. */
  optional: boolean
  integer: boolean
  /** Written in node.toml, rather than the node's default. */
  written: boolean
  /** What the node loads for it now (a secret's never). */
  value: unknown
  /** For a secret: whether node.toml holds one. */
  secretSet: boolean
  /** Whether this browser may change it here. */
  editable: boolean
  /** Why not, when it may not. */
  whyNot?: string
  help?: string
}

/** What a field's control holds while it is edited. `reset` takes the key
    out of node.toml, so the node's default applies again (or, for a
    secret, nothing is held). */
export interface Draft {
  v: string | boolean
  reset?: boolean
}

/** A table of node.toml whose keys are edited together and saved together. */
export interface Section {
  /** The table's dotted path; '' for the keys at the top of the file. */
  id: string
  fields: Field[]
  maps: MapField[]
  arrays: ArrayField[]
}

/** A table whose keys are names the operator picks (`[github.dispatch]`). */
export interface MapField {
  key: string
  segs: string[]
  spec: ConfigKeySpec
  /** What each entry's value is. */
  control: 'text' | 'lines'
  label: string
  entries: [string, unknown][]
  editable: boolean
  whyNot?: string
}

/** An array of tables (`[[service]]`), edited entry by entry. */
export interface ArrayField {
  key: string
  label: string
  /** The schema keys of an entry's fields, relative to the entry. */
  fields: string[]
  entries: Record<string, unknown>[]
  specs: ConfigKeySpec[]
}

/** Arrays and tables edited by their own screens, not by the generic form. */
export const BESPOKE: Record<string, string> = {
  repo: 'Channels & repositories › Repository environments',
  providers: 'Connections › Providers',
}

const BARE = /^[A-Za-z0-9_-]+$/

/** A key path as `PATCH /api/config` reads it: bare keys dotted, any other
    quoted, an array entry by its index. */
export function keyPath(segs: (string | number)[]): string {
  let out = ''
  for (const seg of segs) {
    if (typeof seg === 'number') {
      out += `[${seg}]`
      continue
    }
    if (out) out += '.'
    out += BARE.test(seg) ? seg : `"${seg.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`
  }
  return out
}

/** A key path read back into its table keys, indices dropped: how a refusal
    names a key (`repo[2].egress`, `providers."a.b".upstream`). */
export function keySegs(path: string): string[] {
  const segs: string[] = []
  let i = 0
  while (i < path.length) {
    if (path[i] === '.') {
      i++
      continue
    }
    if (path[i] === '[') {
      i = path.indexOf(']', i) + 1 || path.length
      continue
    }
    let key = ''
    if (path[i] === '"') {
      i++
      while (i < path.length && path[i] !== '"') {
        if (path[i] === '\\') i++
        key += path[i] ?? ''
        i++
      }
      i++
    } else {
      while (i < path.length && path[i] !== '.' && path[i] !== '[') key += path[i++]
    }
    segs.push(key)
  }
  return segs
}

function matches(pattern: string, segs: string[]): boolean {
  const parts = pattern.split('.')
  return parts.length === segs.length && parts.every((p, n) => p === '*' || p === segs[n])
}

/** The schema's entry for a concrete key, `*` standing for a map's names. */
export function specFor(keys: ConfigKeySpec[], segs: string[]): ConfigKeySpec | undefined {
  return keys.find((k) => matches(k.key, segs))
}

/** The value at `segs` in a table, if there is one. */
export function at(table: unknown, segs: (string | number)[]): unknown {
  let v: unknown = table
  for (const seg of segs) {
    if (v === null || typeof v !== 'object') return undefined
    v = (v as Record<string, unknown>)[seg as string]
  }
  return v
}

const UNITS: [RegExp, string][] = [
  [/_secs$/, 'seconds'],
  [/_ms$/, 'milliseconds'],
  [/_bytes$/, 'bytes'],
  [/_tokens$/, 'tokens'],
  [/_per_mtok$/, 'per million tokens'],
]

/** A key's own name, in words: `permission_timeout_secs` is "Permission timeout". */
export function labelOf(name: string): { label: string; unit?: string } {
  let base = name
  let unit: string | undefined
  for (const [re, u] of UNITS) {
    if (re.test(base)) {
      unit = u
      // "budget_tokens" stays a budget of tokens; a timeout drops its unit.
      if (u !== 'tokens') base = base.replace(re, '')
      break
    }
  }
  const words = base.replace(/[_-]+/g, ' ').trim()
  return { label: words.charAt(0).toUpperCase() + words.slice(1), unit }
}

const PATH_HELP: Record<string, string> = {
  executable: 'A program the node runs. It must be out of an agent’s reach; the node checks it again before every run.',
  exec_args: 'Arguments that decide what that program runs, held to a fixed shape.',
  secret_file: 'A credential file the node reads, only from a directory set aside for its secrets.',
  secret_dir: 'A directory of credential files, only one set aside for the node’s secrets.',
  write_dir: 'A directory the node writes into: its own export area, or one marked for it on the host.',
  managed_dir: 'A directory the node replaces whole and serves from.',
  root: 'Where worktrees are made or accepted. Never a system, state or configuration directory.',
  mount: 'Where the state volume is mounted.',
}

function controlOf(shape: ConfigShape | undefined, value: unknown, choices: string[] | undefined): Control {
  if (choices?.length) return 'select'
  switch (shape?.kind) {
    case 'bool':
      return 'bool'
    case 'integer':
    case 'float':
      return 'number'
    case 'list':
      return 'lines'
    case 'enum':
      return 'select'
    case 'string':
      return 'text'
  }
  // `any` (read in its own way) and anything older nodes do not describe:
  // what it holds now decides.
  if (typeof value === 'boolean') return 'bool'
  if (typeof value === 'number') return 'number'
  if (Array.isArray(value)) return 'lines'
  return 'text'
}

/** Whether this browser may change a key, and why not. At the node, a key
    only the host changes still may, but for the state mount. */
export function editability(spec: ConfigKeySpec, local: boolean): { editable: boolean; whyNot?: string } {
  const editable = local ? spec.editable_at_node !== false : spec.editable
  if (editable) return { editable }
  const why = spec.why_not ? `${spec.key} ${spec.why_not}` : 'This key is changed on the host.'
  return { editable, whyNot: local ? why : `Edit at the node: ${why}` }
}

export interface FieldContext {
  keys: ConfigKeySpec[]
  document: Record<string, unknown>
  effective: Record<string, unknown>
  redacted: string
  local: boolean
  /** Choices a key's value is drawn from that the schema cannot know (the
      channels this node holds). */
  choices?: (key: string) => string[] | undefined
}

/** One concrete key as a field, or nothing if the node does not describe it. */
export function fieldFor(ctx: FieldContext, segs: (string | number)[], value?: unknown, written?: boolean): Field | null {
  const names = segs.filter((s): s is string => typeof s === 'string')
  const spec = specFor(ctx.keys, names)
  if (!spec) return null
  const loaded = value !== undefined || written !== undefined ? value : at(ctx.effective, segs)
  const inDoc = written ?? at(ctx.document, segs) !== undefined
  const shape = spec.shape
  const choices = ctx.choices?.(spec.key) ?? (shape?.kind === 'enum' ? shape.choices : undefined)
  const { label, unit } = labelOf(names[names.length - 1])
  const { editable, whyNot } = editability(spec, ctx.local)
  const help = spec.path ? PATH_HELP[spec.path] : undefined
  return {
    key: keyPath(segs),
    segs: names,
    spec,
    control: spec.secret ? 'secret' : controlOf(shape, loaded, choices),
    label,
    unit,
    choices,
    optional: shape?.optional ?? false,
    integer: shape?.kind === 'integer' || (shape?.kind !== 'float' && Number.isInteger(loaded)),
    written: inDoc,
    value: spec.secret ? undefined : loaded,
    secretSet: spec.secret && inDoc,
    editable,
    whyNot,
    help,
  }
}

/** Every table of node.toml the generic form edits, in the schema's order. */
export function sections(ctx: FieldContext): Section[] {
  const out = new Map<string, Section>()
  const section = (id: string) => {
    let s = out.get(id)
    if (!s) {
      s = { id, fields: [], maps: [], arrays: [] }
      out.set(id, s)
    }
    return s
  }
  const seenArrays = new Set<string>()
  for (const spec of ctx.keys) {
    const parts = spec.key.split('.')
    if (BESPOKE[parts[0]]) continue
    if (spec.array) {
      if (seenArrays.has(spec.array)) continue
      seenArrays.add(spec.array)
      const arrayParts = spec.array.split('.')
      const entries = at(ctx.effective, arrayParts)
      section(spec.array).arrays.push({
        key: spec.array,
        label: arrayParts[arrayParts.length - 1],
        fields: ctx.keys.filter((k) => k.array === spec.array).map((k) => k.key.slice(spec.array!.length + 1)),
        specs: ctx.keys.filter((k) => k.array === spec.array),
        entries: Array.isArray(entries) ? (entries as Record<string, unknown>[]) : [],
      })
      continue
    }
    const star = parts.indexOf('*')
    if (star === parts.length - 1) {
      const mapSegs = parts.slice(0, -1)
      const table = at(ctx.effective, mapSegs)
      const { editable, whyNot } = editability(spec, ctx.local)
      section(mapSegs.slice(0, -1).join('.')).maps.push({
        key: keyPath(mapSegs),
        segs: mapSegs,
        spec,
        control: spec.shape?.kind === 'list' ? 'lines' : 'text',
        label: labelOf(mapSegs[mapSegs.length - 1]).label,
        entries: table && typeof table === 'object' ? Object.entries(table as Record<string, unknown>) : [],
        editable,
        whyNot,
      })
      continue
    }
    if (star !== -1) continue
    const field = fieldFor(ctx, parts)
    if (field) section(parts.slice(0, -1).join('.')).fields.push(field)
  }
  return [...out.values()]
}

/** What a field's control starts as: what the node loads, as text where it
    is typed. A secret starts empty, always: its value never reaches here. */
export function initialDraft(field: Field): Draft {
  const v = field.value
  switch (field.control) {
    case 'secret':
      return { v: '' }
    case 'bool':
      return { v: v === true }
    case 'lines':
      return { v: Array.isArray(v) ? v.map(String).join('\n') : '' }
    default:
      return { v: v === undefined || v === null ? '' : typeof v === 'object' ? JSON.stringify(v) : String(v) }
  }
}

/** A field's draft as the value it writes, or why it cannot be written. */
export function parseDraft(field: Field, draft: Draft): { value?: unknown; remove?: true; error?: string } {
  if (draft.reset) return { remove: true }
  const v = draft.v
  switch (field.control) {
    case 'bool':
      return { value: v === true }
    case 'lines':
      return {
        value: String(v)
          .split('\n')
          .map((line) => line.trim())
          .filter(Boolean),
      }
    case 'number': {
      const text = String(v).trim()
      if (!text) {
        // Not set, and not a default either (a key of a table the file
        // leaves out): empty leaves it so.
        return field.optional || field.written || field.value === undefined ? { remove: true } : { error: 'A number is needed here.' }
      }
      const n = Number(text)
      if (!Number.isFinite(n)) return { error: `${text} is not a number.` }
      if (field.integer && !Number.isInteger(n)) return { error: 'A whole number is needed here.' }
      return { value: n }
    }
    case 'secret':
      return String(v) ? { value: String(v) } : {}
    default: {
      const text = String(v)
      if (!text.trim() && field.optional) return { remove: true }
      return { value: field.control === 'text' ? text.trim() : text }
    }
  }
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b)
}

/** Whether a draft would change node.toml. */
export function changed(field: Field, draft: Draft | undefined): boolean {
  if (!draft) return false
  const parsed = parseDraft(field, draft)
  if (parsed.error) return true
  if (parsed.remove) return field.written || (field.control === 'secret' && field.secretSet)
  if (parsed.value === undefined) return false
  if (field.control === 'secret') return true
  // A key with no value at all reads as its control's empty state (off, no
  // lines); left that way, nothing is written.
  if (field.value === undefined && !field.written && same(parsed.value, parseDraft(field, initialDraft(field)).value)) return false
  return !same(parsed.value, field.value)
}

/** The operation a field's draft is saved as, if it changes anything: one
    key set, or one key taken out. Never a table: a table set whole drops
    every key it does not name, a secret among them. */
export function opFor(field: Field, draft: Draft | undefined): ConfigOp | null {
  if (!draft || !changed(field, draft)) return null
  const parsed = parseDraft(field, draft)
  if (parsed.error) return null
  if (parsed.remove) return { op: 'remove', key: field.key }
  return { op: 'set', key: field.key, value: parsed.value }
}

/** Every operation a section's drafts make, and the drafts that cannot be
    written yet, by key. */
export function sectionOps(
  fields: Field[],
  drafts: Record<string, Draft>,
): { ops: ConfigOp[]; errors: Record<string, string> } {
  const ops: ConfigOp[] = []
  const errors: Record<string, string> = {}
  for (const field of fields) {
    const draft = drafts[field.key]
    if (!draft) continue
    const parsed = parseDraft(field, draft)
    if (parsed.error) {
      errors[field.key] = parsed.error
      continue
    }
    const op = opFor(field, draft)
    if (op) ops.push(op)
  }
  return { ops, errors }
}

/** A map's rows as edited: a name and its value's text. */
export interface MapRow {
  name: string
  v: string
}

export function mapRows(map: MapField): MapRow[] {
  return map.entries.map(([name, value]) => ({
    name,
    v: Array.isArray(value) ? value.map(String).join('\n') : String(value ?? ''),
  }))
}

function mapValue(map: MapField, row: MapRow): unknown {
  return map.control === 'lines'
    ? row.v
        .split('\n')
        .map((l) => l.trim())
        .filter(Boolean)
    : row.v
}

/** The operations that make a map hold `rows`: a name set where its value
    changed or it is new, a name taken out where it is gone. */
export function mapOps(map: MapField, rows: MapRow[]): ConfigOp[] {
  const ops: ConfigOp[] = []
  const now = new Map(rows.filter((r) => r.name.trim()).map((r) => [r.name.trim(), mapValue(map, r)]))
  for (const [name] of map.entries) {
    if (!now.has(name)) ops.push({ op: 'remove', key: keyPath([...map.segs, name]) })
  }
  const before = new Map(map.entries)
  for (const [name, value] of now) {
    if (!before.has(name) || !same(before.get(name), value)) {
      ops.push({ op: 'set', key: keyPath([...map.segs, name]), value })
    }
  }
  return ops
}

/** One entry of an array of tables as edited: the entry it started as
    (`origin`, its index when loaded; null for a new one) and what it holds now. */
export interface EntryDraft {
  origin: number | null
  entry: Record<string, unknown>
}

/**
 * The operations that turn the array `array` from `original` into `drafts`,
 * in order: entries gone are removed (last first, so no index shifts under
 * another), the fields of an entry kept are set or removed one by one at its
 * index once the removals are done, new entries are appended, and then, if
 * the order is not already right, one reorder. `original` is compared as
 * the editor would write it, so an entry nobody touched yields nothing.
 */
export function arrayOps(arraySegs: (string | number)[], original: Record<string, unknown>[], drafts: EntryDraft[]): ConfigOp[] {
  return arrayEdit(arraySegs, original, drafts).ops
}

/** `arrayOps`, with where each operation's subject is on screen: a field's
    set names the entry where it sits at that point of the request (after
    the removals, before the reorder), which is not where the form shows it
    once entries move. `screen[i]` is operation i's key as the form spells
    it (`service[2].port` for the third entry shown), so a refusal of the
    operation lands on the field the operator typed into. */
export function arrayEdit(
  arraySegs: (string | number)[],
  original: Record<string, unknown>[],
  drafts: EntryDraft[],
): { ops: ConfigOp[]; screen: (string | undefined)[] } {
  const array = keyPath(arraySegs)
  const ops: ConfigOp[] = []
  const screen: (string | undefined)[] = []
  const push = (op: ConfigOp, shown?: string) => {
    ops.push(op)
    screen.push(shown)
  }
  const kept = new Set(drafts.map((d) => d.origin).filter((o): o is number => o !== null))
  for (let i = original.length - 1; i >= 0; i--) {
    if (!kept.has(i)) push({ op: 'remove', key: `${array}[${i}]` })
  }
  const survivors = original.map((_, i) => i).filter((i) => kept.has(i))
  drafts.forEach((draft, pos) => {
    if (draft.origin === null) return
    const at = survivors.indexOf(draft.origin)
    const before = original[draft.origin] ?? {}
    const names = new Set([...Object.keys(before), ...Object.keys(draft.entry)])
    for (const name of names) {
      const was = before[name]
      const now = draft.entry[name]
      const shown = `${array}[${pos}].${keyPath([name])}`
      if (now === undefined && was !== undefined) {
        push({ op: 'remove', key: `${array}[${at}].${keyPath([name])}` }, shown)
      } else if (now !== undefined && !same(was, now)) {
        push({ op: 'set', key: `${array}[${at}].${keyPath([name])}`, value: now }, shown)
      }
    }
  })
  const fresh = drafts.filter((d) => d.origin === null)
  for (const draft of fresh) push({ op: 'insert', array, entry: draft.entry }, `${array}[${drafts.indexOf(draft)}]`)
  // Where each entry sits once the removals and appends are done.
  const current: EntryDraft[] = [...survivors.map((i) => drafts.find((d) => d.origin === i)!), ...fresh]
  const order = drafts.map((d) => current.indexOf(d))
  if (order.some((from, to) => from !== to)) push({ op: 'reorder', array, order })
  return { ops, screen }
}

/** Re-anchor drafts on a table read again: each entry's origin becomes the
    index of the entry now there with the same identity, or new if none is. */
export function rematch(drafts: EntryDraft[], entries: Record<string, unknown>[], identity: (e: Record<string, unknown>) => string): EntryDraft[] {
  const taken = new Set<number>()
  return drafts.map((d) => {
    const id = identity(d.entry)
    const found = entries.findIndex((e, i) => !taken.has(i) && id !== '' && identity(e) === id)
    if (found === -1) return { ...d, origin: null }
    taken.add(found)
    return { ...d, origin: found }
  })
}

/** How a save went, as a form shows it. */
export type SaveOutcome =
  | { kind: 'saved'; result: ConfigPatchResult }
  /** node.toml changed since it was read: read it again and decide. */
  | { kind: 'conflict'; message: string }
  /** Refused, key by key where the node says which. `reason` is the node's
      kind: `refused` (the path guard, a key only the host changes, one the
      node does not read), `hidden_character`, `too_large`, `invalid`. */
  | { kind: 'refused'; reason: string; message: string; fields: Record<string, string[]>; general: string[] }
  | { kind: 'failed'; message: string }

/** The key a refusal names, as a field key. A place in the request body
    (`ops[3]`, `ops[3].value[1]`, `ops[0].key`) is the key operation 3
    names; an entry being added has no index yet, so its array is named.
    Anything else is a key path, indices kept where the form's keys have
    them. */
export function refusedKey(field: string, ops: ConfigOp[], screen: (string | undefined)[] = []): string {
  const op = field.match(/^ops\[(\d+)\](?:[.[]|$)/)
  if (op) {
    const i = Number(op[1])
    const named = ops[i]
    if (named) return screen[i] ?? ('key' in named ? named.key : named.array)
  }
  // A key exactly as an operation wrote it (`repo[1].dockerfile` after a
  // removal before it): where that operation's subject is on screen.
  const i = ops.findIndex((o) => ('key' in o ? o.key : o.array) === field)
  if (i !== -1 && screen[i]) return screen[i]!
  return field
}

/** What the operator does about a refusal the node gives no field for. */
const REMEDY: Record<string, string> = {
  too_large: 'Save fewer keys at a time: undo some of these edits, save the rest, then make them again.',
  hidden_character:
    'A value holds an invisible or control character (often from pasting) that the passkey prompt could not show. Retype it.',
}

/** Errors by the key of the field they belong to. A refusal's key is
    matched to a field by its table keys, so `repo.dockerfile` (a schema key)
    and `repo[2].dockerfile` both find their field. */
export function errorsByField(
  problems: { field?: string; path?: string; message: string }[],
  ops: ConfigOp[],
  fieldKeys: string[],
  screen: (string | undefined)[] = [],
): { fields: Record<string, string[]>; general: string[] } {
  const fields: Record<string, string[]> = {}
  const general: string[] = []
  for (const p of problems) {
    const raw = refusedKey(p.field ?? p.path ?? '', ops, screen)
    const exact = fieldKeys.find((k) => k === raw)
    const segs = keySegs(raw).join('.')
    const target =
      exact ??
      fieldKeys.find((k) => keySegs(k).join('.') === segs) ??
      // A table set or removed whole: the first field under it.
      fieldKeys.find((k) => keySegs(k).join('.').startsWith(`${segs}.`)) ??
      // A name in a map: the map.
      fieldKeys.find((k) => segs.startsWith(`${keySegs(k).join('.')}.`))
    if (target) (fields[target] ??= []).push(p.message)
    else general.push(p.message)
  }
  return { fields, general }
}

/** Send `ops`, decided against `hash`, and say how it went. A passkey
    prompt, when the node asks for one, happens inside `patch` (`api.call`);
    closing it is a failure like any other. */
export async function saveOps(
  patch: (hash: string, ops: ConfigOp[]) => Promise<ConfigPatchResult>,
  hash: string,
  ops: ConfigOp[],
  fieldKeys: string[],
  screen: (string | undefined)[] = [],
): Promise<SaveOutcome> {
  try {
    return { kind: 'saved', result: await patch(hash, ops) }
  } catch (e) {
    if (e instanceof ApiError) {
      if (e.status === 409) return { kind: 'conflict', message: e.message }
      if (e.status === 422) {
        const reason = e.kind ?? 'refused'
        const problems = (e.fields ?? []).map((p) =>
          // The node names the code point; say what it means where it shows.
          reason === 'hidden_character' ? { ...p, message: `${p.message[0]?.toUpperCase() ?? ''}${p.message.slice(1)}: an invisible or control character. Retype the value.` } : p,
        )
        const { fields, general } = errorsByField(problems, ops, fieldKeys, screen)
        if (!problems.length) general.push(e.message)
        const remedy = REMEDY[reason]
        if (remedy && (!problems.length || general.length)) general.push(remedy)
        return { kind: 'refused', reason, message: e.message, fields, general }
      }
    }
    return { kind: 'failed', message: e instanceof Error ? e.message : String(e) }
  }
}

/** A draft that met a change made elsewhere to the same key. */
export interface Clash {
  key: string
  /** What the node holds now. */
  theirs: unknown
  /** What this form would write. */
  yours: Draft
  /** The line to show, where "now X, yours Y" is not the whole story (an
      entry removed on the node, a name saving would take out). */
  says?: string
}

/**
 * After a conflict, the drafts carried onto node.toml as it is now: a draft
 * of a key nobody else changed is kept as it is; one whose key changed
 * underneath it is kept too, and named, so the operator decides between
 * the two before saving again.
 */
export function rebase(before: Field[], after: Field[], drafts: Record<string, Draft>): { drafts: Record<string, Draft>; clashes: Clash[] } {
  const was = new Map(before.map((f) => [f.key, f]))
  const out: Record<string, Draft> = {}
  const clashes: Clash[] = []
  for (const field of after) {
    const draft = drafts[field.key]
    if (!draft) continue
    const old = was.get(field.key)
    if (!old || !changed(old, draft)) continue
    // The node holds it already (this very save, or someone else's same
    // change): nothing left to write.
    if (field.control !== 'secret' && !changed(field, draft)) continue
    out[field.key] = draft
    const moved = field.control === 'secret' ? old.secretSet !== field.secretSet : !same(old.value, field.value) || old.written !== field.written
    if (moved) clashes.push({ key: field.key, theirs: field.value, yours: draft })
  }
  return { drafts: out, clashes }
}

/** A value as the form shows it in a sentence. */
export function shown(value: unknown): string {
  if (value === undefined || value === null || value === '') return 'nothing'
  if (Array.isArray(value)) return value.length ? value.map(String).join(', ') : 'an empty list'
  return String(value)
}

/** A number of `unit`s in words a person reads at a glance: 86400 seconds is
    "1 day", 52428800 bytes is "50 MiB". Nothing when it adds nothing. */
export function magnitude(value: number, unit?: string): string {
  if (!Number.isFinite(value) || value <= 0) return ''
  if (unit === 'seconds' || unit === 'milliseconds') {
    const secs = unit === 'seconds' ? value : value / 1000
    for (const [size, name] of [
      [86400, 'day'],
      [3600, 'hour'],
      [60, 'minute'],
    ] as const) {
      if (secs >= size) {
        const n = Math.round((secs / size) * 10) / 10
        return `${n} ${name}${n === 1 ? '' : 's'}`
      }
    }
    return unit === 'milliseconds' && secs < 60 ? `${Math.round(secs * 10) / 10} s` : ''
  }
  if (unit === 'bytes' && value >= 1024) {
    const names = ['KiB', 'MiB', 'GiB', 'TiB']
    let n = value / 1024
    let i = 0
    while (n >= 1024 && i < names.length - 1) {
      n /= 1024
      i++
    }
    return `${Math.round(n * 10) / 10} ${names[i]}`
  }
  if (unit === 'tokens' && value >= 1000) {
    return value >= 1e6 ? `${Math.round(value / 1e5) / 10}M tokens` : `${Math.round(value / 100) / 10}k tokens`
  }
  return ''
}

/** The field context of a loaded configuration. */
export function contextOf(cfg: NodeConfig, local: boolean, choices?: FieldContext['choices']): FieldContext {
  return {
    keys: cfg.keys,
    document: cfg.document,
    effective: cfg.effective,
    redacted: cfg.redacted,
    local,
    choices,
  }
}

/** One entry of an array of tables in the generic form: where it started,
    its fields, and what has been typed into them. */
export interface EntryState {
  origin: number | null
  fields: Field[]
  drafts: Record<string, Draft>
}

function entryFields(ctx: FieldContext, array: ArrayField, at: number, entry: Record<string, unknown> | null): Field[] {
  const base = array.key.split('.')
  const out: Field[] = []
  for (const name of array.fields) {
    const segs = name.split('.')
    const value = entry ? at_(entry, segs) : undefined
    const field = fieldFor(ctx, [...base, at, ...segs], value, value !== undefined)
    // An entry's field left empty is left out, and the node says whether
    // the entry needs it: a new entry has no defaults to fall back on here.
    if (field) out.push({ ...field, optional: true })
  }
  return out
}
const at_ = at

/** The entries of an array as the form edits them. */
export function entryStates(ctx: FieldContext, array: ArrayField): EntryState[] {
  return array.entries.map((entry, i) => ({ origin: i, fields: entryFields(ctx, array, i, entry), drafts: {} }))
}

export function blankEntry(ctx: FieldContext, array: ArrayField, at: number): EntryState {
  return { origin: null, fields: entryFields(ctx, array, at, null), drafts: {} }
}

/** What an entry holds once its drafts are applied, and what does not parse. */
export function entryValue(state: EntryState): { entry: Record<string, unknown>; errors: Record<string, string> } {
  const entry: Record<string, unknown> = {}
  const errors: Record<string, string> = {}
  for (const field of state.fields) {
    const name = field.key.replace(/^.*?\[[^\]]*\]\./, '')
    const parsed = parseDraft(field, state.drafts[name] ?? initialDraft(field))
    if (parsed.error) {
      errors[name] = parsed.error
      continue
    }
    if (parsed.remove || parsed.value === undefined) continue
    const segs = keySegs(name)
    let table = entry
    for (const seg of segs.slice(0, -1)) table = (table[seg] ??= {}) as Record<string, unknown>
    table[segs[segs.length - 1]] = parsed.value
  }
  return { entry, errors }
}

/** The operations that make the array hold what its entries now say. */
export function entriesOps(
  array: ArrayField,
  before: EntryState[],
  now: EntryState[],
): { ops: ConfigOp[]; screen: (string | undefined)[]; errors: Record<string, string> } {
  const errors: Record<string, string> = {}
  const original = before.map((s) => entryValue({ ...s, drafts: {} }).entry)
  const drafts: EntryDraft[] = now.map((s, i) => {
    const { entry, errors: e } = entryValue(s)
    for (const [name, message] of Object.entries(e)) errors[`${array.key}[${i}].${name}`] = message
    return { origin: s.origin, entry }
  })
  return { ...arrayEdit(keySegs(array.key), original, drafts), errors }
}

/**
 * An array's entries as edited, carried onto the array as node.toml has it
 * now. The array unchanged, nothing moves. Otherwise each entry the form
 * started from is found again (the same entry, else the same first field as
 * it was loaded, never as edited), takes the fields the node has now and
 * keeps only the drafts typed into it; an entry the node added that the
 * form never saw is kept, at its place; one the node removed that the form
 * still holds is named, with what saving would do. Nobody else's change is
 * undone by saving what comes out, unless a clash says so.
 */
export function rebaseEntries(
  ctx: FieldContext,
  array: ArrayField,
  was: Record<string, unknown>[],
  states: EntryState[],
): { states: EntryState[]; clashes: Clash[] } {
  const now = array.entries
  if (same(was, now)) return { states, clashes: [] }
  const first = array.fields[0]
  const taken = new Set<number>()
  // Where each entry loaded before is now.
  const moved = was.map((entry) => {
    let at = now.findIndex((e, i) => !taken.has(i) && same(e, entry))
    if (at === -1) at = now.findIndex((e, i) => !taken.has(i) && first !== undefined && entry[first] !== undefined && same(e[first], entry[first]))
    if (at !== -1) taken.add(at)
    return at
  })
  const fresh = entryStates(ctx, array)
  const clashes: Clash[] = []
  const out: EntryState[] = []
  for (const st of states) {
    if (st.origin === null) {
      out.push(st)
      continue
    }
    const at = moved[st.origin]
    const label = String(was[st.origin]?.[first] ?? `entry ${st.origin + 1}`)
    if (at === -1) {
      // Removed on the node. Kept as a new entry, and named.
      out.push({ ...st, origin: null, drafts: { ...Object.fromEntries(st.fields.map((f, j) => [array.fields[j], st.drafts[array.fields[j]] ?? initialDraft(f)])) } })
      clashes.push({ key: `${array.key}[${out.length - 1}]`, theirs: undefined, yours: { v: '' }, says: `${label} was removed on the node; saving adds it back with your edits.` })
      continue
    }
    const base = fresh[at]
    for (const [name, draft] of Object.entries(st.drafts)) {
      const j = array.fields.indexOf(name)
      const before = st.fields[j]
      const after = base.fields[j]
      if (before && after && changed(before, draft) && !same(before.value, after.value)) {
        clashes.push({ key: `${array.key}[${out.length}].${name}`, theirs: after.value, yours: draft })
      }
    }
    out.push({ origin: at, fields: base.fields, drafts: st.drafts })
  }
  // Entries the node added: kept where they are.
  now.forEach((_, j) => {
    if (taken.has(j)) return
    out.splice(Math.min(j, out.length), 0, fresh[j])
  })
  return { states: out, clashes }
}

/**
 * A map's rows as edited, carried onto the map as node.toml has it now:
 * only the names the operator set, changed or removed are applied, over
 * what the node holds, so a name someone else added stays and a value
 * someone else changed is not put back. A name both changed is named.
 */
export function rebaseMap(map: MapField, was: [string, unknown][], rows: MapRow[]): { rows: MapRow[]; clashes: Clash[] } {
  const before = new Map(was)
  const now = new Map(map.entries)
  if (same(was, map.entries)) return { rows, clashes: [] }
  const loaded: MapField = { ...map, entries: was }
  const edits = mapOps(loaded, rows)
  const out = new Map(mapRows(map).map((r) => [r.name, r.v]))
  const clashes: Clash[] = []
  for (const op of edits) {
    const name = keySegs(op.op === 'set' || op.op === 'remove' ? op.key : '').at(-1) ?? ''
    const moved = !same(before.get(name), now.get(name))
    if (op.op === 'remove') {
      if (moved && now.has(name)) clashes.push({ key: `${map.key}.${name}`, theirs: now.get(name), yours: { v: '' }, says: `${name} changed on the node to ${shown(now.get(name))}; saving removes it.` })
      out.delete(name)
    } else if (op.op === 'set') {
      if (moved) clashes.push({ key: `${map.key}.${name}`, theirs: now.get(name), yours: { v: shown(op.value) }, says: now.has(name) ? `${name} is now ${shown(now.get(name))} on the node; saving sets ${shown(op.value)}.` : `${name} was removed on the node; saving sets it again.` })
      out.set(name, rows.find((r) => r.name.trim() === name)?.v ?? '')
    }
  }
  return { rows: [...out].map(([name, v]) => ({ name, v })), clashes }
}

/** A provider's table as node.toml must hold it before one key of it is
    written: a built-in provider the file does not name is filled in from
    what the node loads, key by key, so writing `upstream` alone does not
    leave a provider with no credential. Keys `skip` names are left out. */
export function declareOps(ctx: FieldContext, provider: string, skip: string[] = []): ConfigOp[] {
  if (at(ctx.document, ['providers', provider]) !== undefined) return []
  const loaded = at(ctx.effective, ['providers', provider])
  if (!loaded || typeof loaded !== 'object') return []
  const ops: ConfigOp[] = []
  const walk = (prefix: string[], value: Record<string, unknown>) => {
    for (const [name, v] of Object.entries(value)) {
      const segs = [...prefix, name]
      if (name === 'models' && prefix.length === 2) continue
      if (v !== null && typeof v === 'object' && !Array.isArray(v)) {
        walk(segs, v as Record<string, unknown>)
        continue
      }
      const key = keyPath(segs)
      const spec = specFor(ctx.keys, segs)
      if (skip.includes(key) || !spec || spec.secret || v === ctx.redacted) continue
      ops.push({ op: 'set', key, value: v })
    }
  }
  walk(['providers', provider], loaded as Record<string, unknown>)
  return ops
}

/** One declared model as its row edits it: where it started and what it holds. */
export interface ModelRow {
  origin: number | null
  model: Record<string, unknown>
}

/** The reasoning efforts a model declaration may name; the node refuses any other. */
export const REASONING_EFFORTS = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const

function cleanModel(model: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(
    Object.entries(model)
      .map(([k, v]) => [k, typeof v === 'string' ? v.trim() : v] as const)
      .filter(([, v]) => v !== undefined && v !== null && v !== '' && v !== false && v !== 0),
  )
}

/**
 * The operations that make a provider declare `rows`. Where node.toml lists
 * the provider's models, each one is edited where it is (`arrayOps`), a row
 * compared with what it was loaded as so an untouched model writes nothing.
 * Where it does not (a built-in catalogue the node fills in), the list is
 * written whole, once, after the provider's own keys (`declareOps`): a list
 * of models holds no secret.
 */
export function modelOps(ctx: FieldContext, provider: string, rows: ModelRow[]): ConfigOp[] {
  const segs = ['providers', provider, 'models']
  const loaded = (at(ctx.effective, segs) as Record<string, unknown>[] | undefined) ?? []
  const written = at(ctx.document, segs) as Record<string, unknown>[] | undefined
  const now = rows.map((r) => cleanModel(r.model))
  if (!Array.isArray(written)) {
    if (same(now, loaded.map(cleanModel))) return []
    return [...declareOps(ctx, provider), { op: 'set', key: keyPath(segs), value: now }]
  }
  const original = written.map(cleanModel)
  const drafts: EntryDraft[] = rows.map((r, i) => {
    if (r.origin === null) return { origin: null, entry: now[i] }
    // Only what was edited moves off what the file says.
    const base = cleanModel(loaded[r.origin] ?? {})
    const entry = { ...original[r.origin] }
    for (const name of new Set([...Object.keys(base), ...Object.keys(now[i])])) {
      if (same(base[name], now[i][name])) continue
      if (now[i][name] === undefined) delete entry[name]
      else entry[name] = now[i][name]
    }
    return { origin: r.origin, entry }
  })
  return arrayOps(segs, original, drafts)
}
