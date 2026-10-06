// A tool's inputSchema as an editable form, and the check the node repeats
// before it runs edited arguments. Only the JSON Schema subset the brokered
// tools use is understood: type, properties, required, items and enum.

export interface JsonSchema {
  type?: string | string[]
  description?: string
  properties?: Record<string, JsonSchema>
  required?: string[]
  items?: JsonSchema
  enum?: unknown[]
}

export type FieldKind = 'text' | 'prose' | 'integer' | 'number' | 'boolean' | 'select' | 'tags' | 'json'

export interface Field {
  key: string
  kind: FieldKind
  required: boolean
  /** Names the target of the call. The node refuses an edit to it. */
  locked: boolean
  description?: string
  options?: string[]
}

export type FieldValue = string | string[] | boolean

export interface FieldError {
  /** `labels[1]`, `parent`, or '' for the arguments as a whole. */
  path: string
  message: string
}

export const IDENTIFIERS: readonly string[] = ['key', 'project', 'repo', 'number', 'iid']

const PROSE_KEYS = new Set(['body', 'description'])

function typeOf(schema: JsonSchema): string | undefined {
  return Array.isArray(schema.type) ? schema.type.find((t) => t !== 'null') : schema.type
}

function kindOf(key: string, schema: JsonSchema, value: unknown): FieldKind {
  if (schema.enum?.every((v) => typeof v === 'string')) return 'select'
  switch (typeOf(schema)) {
    case 'string':
      return PROSE_KEYS.has(key) || (typeof value === 'string' && value.includes('\n')) ? 'prose' : 'text'
    case 'integer':
      return 'integer'
    case 'number':
      return 'number'
    case 'boolean':
      return 'boolean'
    case 'array':
      return typeOf(schema.items ?? {}) === 'string' ? 'tags' : 'json'
    default:
      return 'json'
  }
}

/** One field per schema property, in the schema's order. */
export function formFields(schema: JsonSchema, args: Record<string, unknown> = {}): Field[] {
  const required = new Set(schema.required ?? [])
  return Object.entries(schema.properties ?? {}).map(([key, prop]) => {
    const kind = kindOf(key, prop, args[key])
    return {
      key,
      kind,
      required: required.has(key),
      locked: IDENTIFIERS.includes(key),
      ...(prop.description ? { description: prop.description } : {}),
      ...(kind === 'select' ? { options: prop.enum as string[] } : {}),
    }
  })
}

/** The form's starting state for `args`. */
export function formValues(fields: Field[], args: Record<string, unknown>): Record<string, FieldValue> {
  return Object.fromEntries(
    fields.map((f) => {
      const v = args[f.key]
      if (f.kind === 'boolean') return [f.key, v === true]
      if (f.kind === 'tags') return [f.key, Array.isArray(v) ? v.map(String) : []]
      if (v === undefined || v === null) return [f.key, '']
      if (f.kind === 'json') return [f.key, JSON.stringify(v, null, 2)]
      return [f.key, String(v)]
    }),
  )
}

/**
 * The arguments the form describes, over `original` so that anything the
 * schema does not name passes through. An empty optional field is left out.
 * A value that does not parse is kept as typed, for `validate` to report.
 */
export function formArguments(
  fields: Field[],
  values: Record<string, FieldValue>,
  original: Record<string, unknown> = {},
): Record<string, unknown> {
  const out: Record<string, unknown> = { ...original }
  for (const f of fields) {
    const v = values[f.key]
    if (v === undefined) continue
    if (f.kind === 'boolean') {
      if (v === false && !f.required && !(f.key in original)) delete out[f.key]
      else out[f.key] = v === true
      continue
    }
    if (f.kind === 'tags') {
      const tags = (Array.isArray(v) ? v : String(v).split(','))
        .map((t) => t.trim())
        .filter(Boolean)
      if (tags.length || f.required || f.key in original) out[f.key] = tags
      else delete out[f.key]
      continue
    }
    const text = String(v)
    if (text.trim() === '' && !f.required) {
      delete out[f.key]
      continue
    }
    if (f.kind === 'integer' && /^\s*-?\d+\s*$/.test(text)) out[f.key] = Number(text)
    else if (f.kind === 'number' && text.trim() !== '' && Number.isFinite(Number(text))) out[f.key] = Number(text)
    else if (f.kind === 'json') {
      try {
        out[f.key] = JSON.parse(text)
      } catch {
        out[f.key] = text
      }
    } else out[f.key] = text
  }
  return out
}

function matchesType(type: string, value: unknown): boolean {
  switch (type) {
    case 'string':
      return typeof value === 'string'
    case 'integer':
      return Number.isInteger(value)
    case 'number':
      return typeof value === 'number' && Number.isFinite(value)
    case 'boolean':
      return typeof value === 'boolean'
    case 'array':
      return Array.isArray(value)
    case 'object':
      return typeof value === 'object' && value !== null && !Array.isArray(value)
    case 'null':
      return value === null
    default:
      return true
  }
}

function child(path: string, key: string | number): string {
  return typeof key === 'number' ? `${path}[${key}]` : path ? `${path}.${key}` : key
}

/** Every way `value` falls outside `schema`, or [] when it fits. */
export function validate(schema: JsonSchema, value: unknown, path = ''): FieldError[] {
  if (schema.type !== undefined) {
    const types = Array.isArray(schema.type) ? schema.type : [schema.type]
    if (!types.some((t) => matchesType(t, value))) {
      return [{ path, message: `must be ${types.map((t) => (/^[aeiou]/.test(t) ? `an ${t}` : `a ${t}`)).join(' or ')}` }]
    }
  }
  const errors: FieldError[] = []
  if (schema.enum && !schema.enum.some((e) => JSON.stringify(e) === JSON.stringify(value))) {
    errors.push({ path, message: `must be one of ${schema.enum.map((e) => JSON.stringify(e)).join(', ')}` })
  }
  if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
    const obj = value as Record<string, unknown>
    for (const key of schema.required ?? []) {
      if (obj[key] === undefined) errors.push({ path: child(path, key), message: 'is required' })
    }
    for (const [key, prop] of Object.entries(schema.properties ?? {})) {
      if (obj[key] !== undefined) errors.push(...validate(prop, obj[key], child(path, key)))
    }
  }
  if (Array.isArray(value) && schema.items) {
    value.forEach((v, i) => errors.push(...validate(schema.items!, v, child(path, i))))
  }
  return errors
}
