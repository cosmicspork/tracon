// The node was upgraded while this page stayed open: its data is current,
// but the interface rendering it is the release that was loaded. Only a
// reload fetches the new one.

/** A release and which build of it; `build` is empty for the release itself. */
export interface BuildIdentity {
  version: string | null | undefined
  /** Absent where the node is too old to say; then only releases are compared. */
  build?: string | null
}

/**
 * Whether the page is not the interface the node now serves. Releases are
 * compared when both are known. Builds are compared too when the node says
 * which one it embeds, since a build of unreleased code carries the version
 * of the release before it.
 */
export function staleInterface(page: BuildIdentity, served: BuildIdentity): boolean {
  if (!page.version || !served.version) return false
  if (page.version !== served.version) return true
  return served.build != null && (page.build ?? '') !== served.build
}

/** "v0.30.0", or "v0.30.0 · dev edb04a39" for a build that is not the release. */
export function versionLabel(version: string | null | undefined, build?: string | null): string {
  if (!version) return ''
  return build ? `v${version} · dev ${build.slice(0, 8)}` : `v${version}`
}
