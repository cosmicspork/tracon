# Phase 2 notes

What implementing the mesh changed, recorded so it is not re-decided. Evidence
gathered on the Linux host (immutable Fedora, rootless Podman 5, netavark + pasta, SELinux
enforcing) on 2026-08-28, with the macOS Podman-machine node as the first peer.

## The Linux gateway forward is a Unix socket

`host.containers.internal` under pasta resolves to `169.254.1.2`, a host
interface address, not loopback. A node listener bound to `127.0.0.1:7421` is
therefore unreachable from the gateway container:

```
$ podman run --rm alpine sh -c 'getent hosts host.containers.internal; wget -qO- http://host.containers.internal:7421/'
169.254.1.2       host.containers.internal
wget: can't connect to remote host (169.254.1.2): Connection refused
```

The macOS path works only because gvproxy forwards to the VM host's loopback.
So on Linux the design-doc path is implemented: the harness listener is a Unix
socket at `$XDG_RUNTIME_DIR/tracon/harness.sock`, the gateway mounts that
directory at `/run/tracon` (relabelled `:z` when SELinux is enforcing, since the
gateway is trusted and the directory is private), and socat forwards
`UNIX-CONNECT:/run/tracon/harness.sock`. `[gateway] harness_listen` accepts
either a socket address or an absolute path; the default is the socket on
Linux and `127.0.0.1:7421` elsewhere. Binding a TCP listener on the host-side
pasta address was rejected: it would expose the harness listener to the LAN,
guarded only by the per-session bearer token.

Under SELinux two more things were needed, found by running it:

- The gateway's bind mounts need relabelling. The first run under SELinux died
  with `allow.txt missing`: the file was mounted but unreadable from the
  container's context. The allowlist file and the socket directory are mounted
  with `:z` when `podman info` reports SELinux; they are the node's own state.
- Relabelling is not enough for the socket itself. Connecting to a Unix socket
  is checked against the *listener's* process label (`connectto`), and policy
  forbids `container_t` → `unconfined_t` whatever the file is labelled; socat
  logged `connect(... "/run/tracon/harness.sock"): Permission denied` with the
  socket already `container_file_t`. The gateway therefore runs with
  `--security-opt label=disable` when SELinux is enforcing and the forward is a
  socket. That is consistent with the design: the gateway is the trusted,
  node-owned piece and exists so the harness never touches the socket; the
  harness container keeps its confinement, `--cap-drop=ALL`, and
  `no-new-privileges` exactly as before.

`tracon check-boundary --deep` now also asserts that the node answers
`/harness/ping` through the forward, so a broken forward fails the check rather
than passing on egress alone.

Verified on the Linux host on 2026-08-28: `tracon setup` built both images
from the embedded definitions and started the gateway; `tracon check-boundary
--deep` passed all five checks; a probe container on the internal network got
`pong` from `http://tracon-gw:7421/harness/ping` and a 404 from
`/api/node` on the same forward (the harness router carries no operator API).

## `OMP_STATE_DIR` is set but unverified

The design says to copy omp's `OMP_STATE_DIR` pattern. The omp 18 binary is a
compiled bundle with its strings compressed, so whether it honours the variable
cannot be confirmed by inspection, and `omp --version` touches no state
directory either way. The node keeps mounting the node-owned volume at
`/root/.omp` (proven in Phase 1) and additionally sets `OMP_STATE_DIR=/root/.omp`
in the runner, which is a no-op if omp ignores it. If a future omp moves its
state directory, the env var is already in place and only the mount target needs
to follow.

## The harness volume is authoritative

Phase 1 bind-mounted the operator's `~/.omp/agent/agent.db` (and WAL/SHM) into
the node-owned state directory on every run. Phase 2 stops that: the volume is
the only credential store the harness sees. `tracon harness import-credentials`
copies a credential database in once, through SQLite's online backup so a live
WAL is folded into a single consistent file; `tracon harness shell` runs the
harness image on the default network with only the volume mounted, for a
one-time `omp` login on a host that has no operator install at all. A node with
no credentials in the volume reports `harness_credentials: false`, skips the
model probe (which would otherwise fail and look like a harness fault), and says
what to run.

## Images ship inside the binary

`tracon setup` on a binary-only host needs the gateway and harness images.
`containers/` is embedded into the node binary with `rust-embed`, written to
`<state>/containers/`, and built with `podman build` when the configured image
is absent (`--rebuild` forces it). The harness image still fetches the pinned
omp release at build time, which is the one network fetch a fresh node makes.
