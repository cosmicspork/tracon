import type { DocKindInfo, Document } from './types'

/** A slug's kind under the node's list, as the node classifies it: the
 *  kind whose prefix it is or starts with followed by `-`, else `other`. */
export function kindOf(slug: string, kinds: DocKindInfo[]): string {
  const hit = kinds.find((k) => k.kind !== 'other' && (slug === k.kind || slug.startsWith(`${k.kind}-`)))
  return hit?.kind ?? 'other'
}

/** What the Documents list calls a kind: the node's label, else the kind. */
export function kindLabel(kind: string, kinds: DocKindInfo[]): string {
  return kinds.find((k) => k.kind === kind)?.label || kind
}

/** Live documents by kind in the node's order (`GET /api/docs/kinds`), then
 *  archived ones as their own group. A kind the list does not name still gets
 *  a group, after the named ones and before `other`: a document must not
 *  vanish from the list because the interface was not told about its kind. */
export function groupDocs(docs: Document[], kinds: DocKindInfo[]): (readonly [string, Document[]])[] {
  const live = new Map<string, Document[]>()
  const archived: Document[] = []
  for (const d of docs) {
    if (d.archived) archived.push(d)
    else live.set(d.kind, [...(live.get(d.kind) ?? []), d])
  }
  const order = kinds.map((k) => k.kind)
  const other = order.includes('other') ? order.indexOf('other') : order.length
  const rank = (kind: string) => {
    if (kind === 'other') return other
    const i = order.indexOf(kind)
    return i === -1 ? other - 0.5 : i
  }
  const groups = [...live.entries()]
    .sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b))
    .map(([kind, list]) => [kind, list] as const)
  return archived.length ? [...groups, ['archived', archived] as const] : groups
}
