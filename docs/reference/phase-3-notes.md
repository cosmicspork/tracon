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

## Lab proof (homelab, Cilium, k8s 1.35), 2026-08-28

`kubectl apply -k deploy/kubernetes/lab` into `tracon-lab` (outside Flux), image
`ghcr.io/cosmicspork/tracon-node:0.2.0`, harness `tracon-harness:0.2.0`. Two findings
on the first run, both fixed the same day:

1. **A gated pod may not carry `nodeName`.** The API refuses it ("cannot be set until
   all schedulingGates have been cleared"), so the probe could not be created and every
   static check failed closed — the right failure, for the wrong reason. The probe now
   pins by `nodeSelector: kubernetes.io/hostname`; session pods keep `nodeName`.
2. **Attach is a `GET`.** kube-rs opens `pods/attach` as a WebSocket upgrade on GET,
   which the API server authorises as the `get` verb; the Role granted only `create`
   and the runtime check asserted only what the Role had, so the node could create a
   harness it could not talk to (`403 Forbidden` on upgrade). Both the Role and the
   `SelfSubjectAccessReview` list now carry `create` and `get`.

With those, inside the node pod:

```
ok   runtime                api reachable; pods/attach/log and networkpolicies granted in tracon-lab; node general-ihxil
ok   harness_unprivileged   non-root uid 65532, no capabilities, no escalation, seccomp RuntimeDefault
ok   no_runtime_socket      no host namespaces, no API token, no hostPath; only the state claim
ok   network_isolated       no resolver; tracon-harness allows egress only to the node pod on the forward and proxy ports
ok   egress                 no direct egress; allowlisted host reachable, unlisted host refused
```

The deep probe ran as a real harness pod under the NetworkPolicy: `curl --noproxy '*'
https://example.com` failed (no route, no resolver), `https://api.anthropic.com` through
`tracon-gw:8888` succeeded, `https://example.com` through the proxy was refused, and
`http://tracon-gw:7421/harness/ping` answered `pong` — the node's own proxy and forward,
reached by pod IP, with nothing else reachable.

A session (`POST /api/sessions`, repo cloned onto the volume) then:

- created the worktree at `/state/work/<repo>-<slug>` on the shared claim;
- created `tracon-h-<slug>` with the full mount layout as `subPath`s of the claim —
  `/work` rw, `<repo>/.git` rw at its absolute path, and `.git/config`, `hooks`, `info`
  layered read-only over it. **kubelet accepts the nested read-only subPath mounts**; the
  fallback in the plan was not needed;
- attached, completed the ACP `initialize` / `session/new` handshake with omp inside the
  pod, and failed honestly at model selection ("not offered by the harness") because the
  lab volume holds no harness credentials. That is the exec pipe proven end to end; a
  full turn needs `tracon harness import-credentials` against a copied `agent.db`.

The exit criterion's network half — an agent in the runner cannot reach GitLab, Jira, or
a database except through the node — is the `egress` line above under a policy that
names the node pod as the only peer. The tool half (`mr_status`, `issue_comment`, …)
is exercised against API stubs in `node/tests/work_tools.rs`; the verbs that would merge
or transition do not exist to call.

Not done in the lab: a turn with model credentials, and the Coder template itself
(`deploy/coder/README.md` is what it is written against).
