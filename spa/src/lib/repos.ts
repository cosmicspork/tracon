// The repository table as a form: what the operator types, and the `[[repo]]`
// entries it stands for. Kept apart from the component so the round trip —
// which is where an entry would silently change meaning — is tested.

import { arrayEdit } from './config-form'
import type { ConfigOp, RepoEntry, RepoImageBuild } from './types'

export interface RepoForm {
  path: string
  /** Where the image comes from: the harness's own, one pinned by hand, or built. */
  source: 'harness' | 'image' | 'dockerfile'
  image: string
  dockerfile: string
  context: string
  /** Off: the node-wide checks. On: `checks`, which may be empty on purpose. */
  ownChecks: boolean
  /** One command per line. */
  checks: string
  timeout: string
  /** One command per line. */
  prepare: string
  /** Presets and host names, separated by spaces or commas. */
  egress: string
  sessionEgress: boolean
  /** What the entry says that this form does not edit (`commits`, `style`),
   *  carried back as it came, so comparing the entry with what was loaded
   *  finds no change in them. */
  kept: Partial<RepoEntry>
  /** The index the entry had in the table this form was seeded from; null
   *  for one added here. A save edits each entry where it is. */
  origin?: number | null
}

export function lines(text: string): string[] {
  return text
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
}

export function words(text: string): string[] {
  return text.split(/[\s,]+/).filter(Boolean)
}

export function blankForm(): RepoForm {
  return {
    path: '',
    source: 'dockerfile',
    image: '',
    dockerfile: '.devcontainer/Dockerfile',
    context: '',
    ownChecks: false,
    checks: '',
    timeout: '',
    prepare: '',
    egress: '',
    sessionEgress: false,
    kept: {},
    origin: null,
  }
}

export function toForm(entry: RepoEntry): RepoForm {
  const { path, image, dockerfile, context, checks, timeout_secs, prepare, egress, session_egress, ...kept } = entry
  return {
    path,
    source: dockerfile ? 'dockerfile' : image ? 'image' : 'harness',
    image: image ?? '',
    dockerfile: dockerfile ?? '',
    context: context ?? '',
    ownChecks: checks !== undefined && checks !== null,
    checks: (checks ?? []).join('\n'),
    timeout: timeout_secs ? String(timeout_secs) : '',
    prepare: prepare.join('\n'),
    egress: egress.join(' '),
    sessionEgress: session_egress === true,
    kept,
  }
}

/// The entry a form stands for. A field the form's choices make meaningless
/// is left out rather than sent: an image typed and then abandoned for a
/// Dockerfile must not come back as an entry that names both.
export function toEntry(form: RepoForm): RepoEntry {
  const entry: RepoEntry = {
    ...form.kept,
    path: form.path.trim(),
    prepare: lines(form.prepare),
    egress: words(form.egress),
  }
  if (form.source === 'image' && form.image.trim()) entry.image = form.image.trim()
  if (form.source === 'dockerfile' && form.dockerfile.trim()) {
    entry.dockerfile = form.dockerfile.trim()
    if (form.context.trim()) entry.context = form.context.trim()
  }
  if (form.ownChecks) entry.checks = lines(form.checks)
  const timeout = Number(form.timeout)
  if (form.timeout.trim() && Number.isFinite(timeout) && timeout > 0) entry.timeout_secs = Math.floor(timeout)
  if (form.sessionEgress) entry.session_egress = true
  return entry
}

/// What would be refused, said before the node has to: the first problem per
/// entry, in the operator's terms. The node validates the rest (a digest's
/// shape, a host name) and its message is shown as it is.
export function problems(forms: RepoForm[]): string[] {
  const found: string[] = []
  const seen = new Set<string>()
  for (const form of forms) {
    const path = form.path.trim()
    const named = path || 'an entry'
    if (!path) found.push('Every entry needs the repository it is for.')
    else if (seen.has(path)) found.push(`${path} is named twice; the first entry would always win.`)
    else if (form.source === 'image' && !form.image.trim()) found.push(`${named}: name the image, or choose another source.`)
    else if (form.source === 'dockerfile' && !form.dockerfile.trim()) found.push(`${named}: name the Dockerfile to build.`)
    else if (form.sessionEgress && words(form.egress).length === 0)
      found.push(`${named}: sessions are given its egress, and its egress is empty.`)
    seen.add(path)
  }
  return found
}

/// The table as the form would save it, for comparing with what is loaded:
/// the node's JSON orders an entry's keys its own way and states some things
/// the form says differently, so both sides go through the same round trip.
export function canonical(entries: RepoEntry[]): string {
  return JSON.stringify(entries.map((entry) => toEntry(toForm(entry))))
}

/// The entries in `egress` that reach a host an upload could go to. Said
/// beside "open to sessions", because that is where it matters: a session
/// runs for hours on what a model decides.
export function acceptsUploads(egress: string[], presets: [string, string[]][]): string[] {
  const hosts = (name: string) => presets.find(([preset]) => preset === name)?.[1] ?? [name]
  return egress.filter((name) => hosts(name).some((host) => host === 'api.github.com' || host === 'github.com'))
}

/// One line for a build: what it is and how it stands.
export function buildLine(build: RepoImageBuild): string {
  const what =
    build.kind === 'base'
      ? 'image'
      : build.kind === 'trial'
        ? 'image tried by a session'
        : `session image (${build.kind.replace('session:', '')})`
  const from = build.source_ref ? ` from ${build.source_ref} ${build.source_commit.slice(0, 12)}` : ''
  if (build.status === 'building') return `${what}${from} · building…`
  if (build.status === 'failed') return `${what}${from} · failed: ${build.error}`
  return `${what}${from} · ready`
}

/// The operations a save sends (`PATCH /api/config`): entries removed,
/// fields of a kept entry set or removed where it is, entries added, then
/// the order. Each side goes through the form's round trip first, so an
/// entry nobody touched sends nothing. `screen[i]` is where operation i's
/// subject is on screen (`repo[2].egress` for the third form), for placing
/// a refusal.
export function repoEdit(seeded: RepoEntry[], forms: RepoForm[]): { ops: ConfigOp[]; screen: (string | undefined)[] } {
  const original = seeded.map((entry) => toEntry(toForm(entry)) as unknown as Record<string, unknown>)
  return arrayEdit(
    ['repo'],
    original,
    forms.map((form) => ({ origin: form.origin ?? null, entry: toEntry(form) as unknown as Record<string, unknown> })),
  )
}

export function repoOps(seeded: RepoEntry[], forms: RepoForm[]): ConfigOp[] {
  return repoEdit(seeded, forms).ops
}

/// `[[repo]]` as node.toml writes it (`GET /api/config` `document`), in the
/// shape the form reads: the lists an entry may leave out, empty.
export function writtenRepos(document: Record<string, unknown>): RepoEntry[] {
  const table = document.repo
  if (!Array.isArray(table)) return []
  return table.map((entry) => ({ prepare: [], egress: [], ...(entry as Partial<RepoEntry>) }) as RepoEntry)
}

/// What the form says, one field at a time.
const FORM_FIELDS = ['path', 'source', 'image', 'dockerfile', 'context', 'ownChecks', 'checks', 'timeout', 'prepare', 'egress', 'sessionEgress', 'kept'] as const

/// The forms, carried onto the table as the node has it now (`now`), when
/// they were edited against `seeded`. Only what the operator changed in a
/// form is replayed, over the entry as it is now, so another writer's change
/// to a field nobody touched here stays. An entry the node added is kept at
/// its place; one the operator removed stays removed; one the node removed
/// is dropped from the forms and named. A field both changed differently is
/// named too: saving writes the form's.
export function replayRepos(
  seeded: RepoEntry[],
  forms: RepoForm[],
  now: RepoEntry[],
): { forms: RepoForm[]; removed: string[]; clashes: string[] } {
  const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b)
  const taken = new Set<number>()
  const moved = seeded.map((entry) => {
    let at = now.findIndex((e, i) => !taken.has(i) && same(e, entry))
    if (at === -1) at = now.findIndex((e, i) => !taken.has(i) && e.path === entry.path)
    if (at !== -1) taken.add(at)
    return at
  })
  const out: RepoForm[] = []
  const removed: string[] = []
  const clashes: string[] = []
  for (const form of forms) {
    if (form.origin === null || form.origin === undefined) {
      out.push(form)
      continue
    }
    const was = seeded[form.origin]
    const at = moved[form.origin]
    if (!was) continue
    if (at === -1) {
      removed.push(was.path)
      continue
    }
    const base = toForm(was)
    const theirs = toForm(now[at])
    const next: RepoForm = { ...theirs, origin: at }
    for (const field of FORM_FIELDS) {
      if (same(form[field], base[field])) continue
      ;(next as unknown as Record<string, unknown>)[field] = form[field]
      if (!same(theirs[field], base[field]) && !same(theirs[field], form[field])) clashes.push(`${was.path}: ${field}`)
    }
    out.push(next)
  }
  now.forEach((entry, j) => {
    if (!taken.has(j)) out.splice(Math.min(j, out.length), 0, { ...toForm(entry), origin: j })
  })
  return { forms: out, removed, clashes }
}

/// The fields of entry `index` as a refusal may name them.
export const REPO_FIELDS = ['path', 'image', 'dockerfile', 'context', 'checks', 'timeout_secs', 'prepare', 'egress', 'session_egress']
