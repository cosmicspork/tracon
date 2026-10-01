// The repository table as a form: what the operator types, and the `[[repo]]`
// entries it stands for. Kept apart from the component so the round trip —
// which is where an entry would silently change meaning — is tested.

import type { RepoEntry, RepoImageBuild } from './types'

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
  }
}

export function toForm(entry: RepoEntry): RepoForm {
  return {
    path: entry.path,
    source: entry.dockerfile ? 'dockerfile' : entry.image ? 'image' : 'harness',
    image: entry.image ?? '',
    dockerfile: entry.dockerfile ?? '',
    context: entry.context ?? '',
    ownChecks: entry.checks !== undefined && entry.checks !== null,
    checks: (entry.checks ?? []).join('\n'),
    timeout: entry.timeout_secs ? String(entry.timeout_secs) : '',
    prepare: entry.prepare.join('\n'),
    egress: entry.egress.join(' '),
    sessionEgress: entry.session_egress === true,
  }
}

/// The entry a form stands for. A field the form's choices make meaningless
/// is left out rather than sent: an image typed and then abandoned for a
/// Dockerfile must not come back as an entry that names both.
export function toEntry(form: RepoForm): RepoEntry {
  const entry: RepoEntry = {
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

/// The entries in `egress` that reach a host an upload could go to. Said
/// beside "open to sessions", because that is where it matters: a session
/// runs for hours on what a model decides.
export function acceptsUploads(egress: string[], presets: [string, string[]][]): string[] {
  const hosts = (name: string) => presets.find(([preset]) => preset === name)?.[1] ?? [name]
  return egress.filter((name) => hosts(name).some((host) => host === 'api.github.com' || host === 'github.com'))
}

/// One line for a build: what it is and how it stands.
export function buildLine(build: RepoImageBuild): string {
  const what = build.kind === 'base' ? 'image' : `session image (${build.kind.replace('session:', '')})`
  const from = build.source_ref ? ` from ${build.source_ref} ${build.source_commit.slice(0, 12)}` : ''
  if (build.status === 'building') return `${what}${from} · building…`
  if (build.status === 'failed') return `${what}${from} · failed: ${build.error}`
  return `${what}${from} · ready`
}
