// Readable labels for things the node names by a derived id: a doc by its
// title (a plan's slug is derived from its task id) and a memory's project by
// its remote (a project id is sha256 of the channel and the remote). Each is
// read once per page load and kept.

import { SvelteMap } from 'svelte/reactivity'

import { api } from './api'

const docTitles = new SvelteMap<string, string | null>()
const askedDocs = new Set<string>()

/** The doc's title, or null until it is known (or when it cannot be read). */
export function docTitle(channel: string, slug: string): string | null {
  const key = `${channel}/${slug}`
  if (!askedDocs.has(key)) {
    askedDocs.add(key)
    api
      .doc(channel, slug)
      .then((doc) => docTitles.set(key, doc.title || null))
      .catch(() => docTitles.set(key, null))
  }
  return docTitles.get(key) ?? null
}

const projects = new SvelteMap<string, Map<string, string>>()
const askedChannels = new Set<string>()

/**
 * A project as a memory's scope reads: its canonical remote, or its name when
 * it has none, or null while the channel's projects load or when this node
 * does not list it.
 */
export function projectLabel(channel: string, projectId: string): string | null {
  if (!askedChannels.has(channel)) {
    askedChannels.add(channel)
    api
      .projects(channel)
      .then((r) => projects.set(channel, new Map(r.projects.map((p) => [p.id, p.remote_url || p.name]))))
      .catch(() => projects.set(channel, new Map()))
  }
  return projects.get(channel)?.get(projectId) ?? null
}
