# Phase 3 notes: the work node enforces

Evidence and findings from building the pod-hosted boundary and the work-side
broker. The plan's decisions (2026-08-28): the Coder template itself is written on a
host that can reach that environment, from `deploy/coder/README.md`; the runner
topology is harness pods behind a NetworkPolicy; forge and tracker tools are narrow
MCP verbs over REST.

## What the Podman boundary could not do in a pod

The Phase 1 boundary shells out to `podman` for setup, checks, spawning, and
reconciliation. A Coder workspace pod has no container runtime and, in the current
template, is privileged — so neither half of "the node owns an isolated runner" can
be satisfied there. The seam (`boundary::Backend`) is what lets the same node binary
answer the same five checks from Kubernetes objects instead.

## Kubernetes as the boundary

| Podman | Kubernetes |
|---|---|
| internal network, `--disable-dns` | NetworkPolicy on `tracon.dev/role=harness`; `dnsPolicy: None` |
| gateway container: tinyproxy + socat forward | the node itself: `gateway/proxy.rs` on 8888, harness router on `0.0.0.0:7421` |
| `--cap-drop=ALL`, `no-new-privileges` | `drop ALL`, `allowPrivilegeEscalation: false`, non-root, seccomp |
| bind mounts | `subPath`s of one RWO claim; a source outside it cannot be mounted |
| probe container created, never started | probe pod with a scheduling gate: admitted, never scheduled |
| `podman run -i` stdio | `pods/attach` |

## Lab proof (homelab, Cilium)

_Pending the first release that publishes `ghcr.io/cosmicspork/tracon-node` and
`tracon-harness`._ The steps and what each is expected to show:

1. `kubectl apply -k deploy/kubernetes/lab` — namespace `tracon-lab`, outside Flux.
2. `tracon check-boundary --deep` inside the node pod — all five checks.
3. Nested read-only `subPath` mounts over a read-write one (`.git/config`, `hooks`,
   `info` over `.git`) — kubelet accepts them, or the recorded fallback applies.
4. A session on a small public repository from the port-forwarded interface.
5. From inside the harness pod: `curl https://gitlab.com` fails; `mr_status` through
   the node succeeds.
