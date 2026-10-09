import type { Document } from './types'

// The order the Documents list shows kinds in. The node decides a document's
// kind (`mcp::docs::KINDS`, else `other`); this only orders them.
export const DOC_KINDS = [
  'guide',
  'ref',
  'architecture',
  'brief',
  'context',
  'plan',
  'proposal',
  'repo',
  'note',
  'meeting',
  'inbox',
  'shown',
  'other',
]

/** Live documents by kind in `DOC_KINDS` order, then archived ones as their
 *  own group. A kind this list does not name still gets a group, after the
 *  named ones and before `other`: a kind the node adds must not vanish from
 *  the list because the interface was not told about it. */
export function groupDocs(docs: Document[]): (readonly [string, Document[]])[] {
  const live = new Map<string, Document[]>()
  const archived: Document[] = []
  for (const d of docs) {
    if (d.archived) archived.push(d)
    else live.set(d.kind, [...(live.get(d.kind) ?? []), d])
  }
  const other = DOC_KINDS.indexOf('other')
  const rank = (kind: string) => {
    const i = DOC_KINDS.indexOf(kind)
    return i === -1 ? other - 0.5 : i
  }
  const groups = [...live.entries()]
    .sort(([a], [b]) => rank(a) - rank(b) || a.localeCompare(b))
    .map(([kind, list]) => [kind, list] as const)
  return archived.length ? [...groups, ['archived', archived] as const] : groups
}
