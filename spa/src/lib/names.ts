// Generated display names, `<kind>-<adjective>-<noun>-<NN>`: the label the
// interface shows in place of a raw id. The derivation is the node's
// (`proto/src/names.rs`) and both are pinned by `spec/vectors/names.json`, so a
// name read here is the one the node writes and the CLI prints.
//
// FNV-1a-64 rather than SHA-256 because a name is computed while rendering and
// `crypto.subtle` is async-only. A name is a label, never a key: two records
// may share one, so a link still carries the id.

import { SvelteMap } from 'svelte/reactivity'

import words from '../../../proto/src/names-v1.json'

/** The kinds that carry a generated name; the kind is part of the hash. */
export type NameKind =
  | 'session'
  | 'review'
  | 'report'
  | 'approval'
  | 'manifest'
  | 'agent'
  | 'task'
  | 'transfer'
  | 'check'
  | 'node'

export const DOMAIN = 'tracon/name/v1'

const ADJECTIVES: readonly string[] = words.adjectives
const NOUNS: readonly string[] = words.nouns
const A = BigInt(ADJECTIVES.length)
const N = BigInt(NOUNS.length)

const MASK = (1n << 64n) - 1n
const FNV_OFFSET = 0xcbf29ce484222325n
const FNV_PRIME = 0x100000001b3n
const encoder = new TextEncoder()

function fnv1a(h: bigint, bytes: Uint8Array): bigint {
  for (const b of bytes) h = ((h ^ BigInt(b)) * FNV_PRIME) & MASK
  return h
}

/** splitmix64's mix step, without the golden-ratio increment. */
function splitmix64(z: bigint): bigint {
  z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & MASK
  z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & MASK
  return z ^ (z >> 31n)
}

/** The 64-bit hash a name is picked from. */
export function nameHash(kind: string, id: string): bigint {
  return splitmix64(fnv1a(FNV_OFFSET, encoder.encode(`${DOMAIN}\x1f${kind}\x1f${id}`)))
}

const cache = new Map<string, string>()

/** The generated name of the record of `kind` whose full id is `id`. */
export function generatedName(kind: NameKind, id: string): string {
  const key = `${kind}\x1f${id}`
  let name = cache.get(key)
  if (name === undefined) {
    const h = nameHash(kind, id)
    const nn = String((h / (A * N)) % 100n).padStart(2, '0')
    name = `${kind}-${ADJECTIVES[Number(h % A)]}-${NOUNS[Number((h / A) % N)]}-${nn}`
    if (cache.size > 5000) cache.clear()
    cache.set(key, name)
  }
  return name
}

/** The kinds the operator can rename. A task's title is its name; a node is named in `node.toml`. */
export const RENAMABLE: readonly NameKind[] = ['session', 'review', 'report', 'approval', 'manifest', 'transfer', 'check']

// The operator's names, from `GET /api/names` and then `name` frames.
// Reactive, so every `nameOf` on the page follows a rename made in another
// tab or on another node. A review and a report share a table and a page, so
// they share a key: one id is never both.
const operatorNames = new SvelteMap<string, string>()

function overrideKey(kind: string, id: string): string {
  return `${kind === 'report' ? 'review' : kind}\x1f${id}`
}

/** The operator's name for a record, if it has one. */
export function operatorName(kind: NameKind, id: string): string | undefined {
  return operatorNames.get(overrideKey(kind, id))
}

/** How a record reads: the operator's name, else its generated name. */
export function nameOf(kind: NameKind, id: string): string {
  return operatorName(kind, id) ?? generatedName(kind, id)
}

/** Replace every operator name with the node's list. */
export function setOperatorNames(list: readonly { kind: string; id: string; name: string }[]): void {
  const next = new Map(list.map((n) => [overrideKey(n.kind, n.id), n.name]))
  for (const key of [...operatorNames.keys()]) if (!next.has(key)) operatorNames.delete(key)
  for (const [key, name] of next) if (operatorNames.get(key) !== name) operatorNames.set(key, name)
}

/** One name set, or reverted with null, as a `name` frame or a rename's answer says. */
export function applyOperatorName(kind: string, id: string, name: string | null | undefined): void {
  const key = overrideKey(kind, id)
  if (name) operatorNames.set(key, name)
  else operatorNames.delete(key)
}

/**
 * Whether a route segment is a record's id rather than a name to resolve: a
 * UUID, or 64 hex characters (a task id, a digest).
 */
export function looksLikeId(segment: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(segment) || /^[0-9a-f]{64}$/i.test(segment)
}

/**
 * Why a name cannot be one, as the node checks it, or null when it can: one
 * line of 1 to 64 characters with no control characters, and not all hex
 * (dashes allowed) at 8 or more hex characters, which would read as an id.
 */
export function nameProblem(raw: string): string | null {
  const name = raw.trim()
  if (!name) return 'A name cannot be empty.'
  if ([...name].length > 64) return 'A name is at most 64 characters.'
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f]/.test(name)) return 'A name is one line with no control characters.'
  if (/^[0-9a-f-]+$/i.test(name) && name.replace(/-/g, '').length >= 8) return 'A name that is all hex reads as an id; add a word.'
  return null
}

/** The words a name is built from, for the list's own tests. */
export const nameWords = { adjectives: ADJECTIVES, nouns: NOUNS }

/**
 * How a node reads anywhere it is referred to: its `node.toml` name when this
 * node knows it, its generated `node-…` name when it does not.
 */
export function nodeDisplayName(nodes: readonly { id: string; name?: string | null }[], id: string): string {
  return nodes.find((node) => node.id === id)?.name || generatedName('node', id)
}

/**
 * A content hash or version digest, which is a version rather than a record and
 * gets no name: its first 8 characters. The full value goes in a tooltip and on
 * copy.
 */
export function shortHash(hash: string): string {
  return hash.slice(0, 8)
}
