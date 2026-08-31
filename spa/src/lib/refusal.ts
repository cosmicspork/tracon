// What to do about a node that will not run harnesses. The node names the
// check that failed and says what it saw; this says what fixes it, because a
// check id and an error string are a diagnosis, not an instruction.

const REMEDIES: Record<string, string> = {
  runtime:
    'start the container runtime, or set `podman` under [boundary] in node.toml, then restart the node',
  harness_unprivileged: 'run `tracon setup` to rebuild the harness image, then restart the node',
  no_runtime_socket: 'run `tracon setup` to rebuild the harness image, then restart the node',
  network_isolated: 'run `tracon setup` to recreate the network and gateway, then restart the node',
  egress: 'run `tracon setup` to rebuild the gateway, then restart the node',
}

/** The remedy for a failed boundary check, or a way to find out. */
export function remedy(check: string | null | undefined): string {
  if (!check) return 'run `tracon check-boundary --deep` at the node for detail'
  return REMEDIES[check] ?? 'run `tracon check-boundary --deep` at the node for detail'
}
