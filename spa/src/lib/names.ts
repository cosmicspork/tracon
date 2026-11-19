// Generated display names, `<kind>-<adjective>-<noun>-<NN>`: the label the
// interface shows in place of a raw id. The derivation is the node's
// (`proto/src/names.rs`) and both are pinned by `spec/vectors/names.json`, so a
// name read here is the one the node writes and the CLI prints.
//
// FNV-1a-64 rather than SHA-256 because a name is computed while rendering and
// `crypto.subtle` is async-only. A name is a label, never a key: two records
// may share one, so a link still carries the id.

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
