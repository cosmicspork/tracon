# The work node on Coder

What the Coder template must produce for a workspace to be a tracon node. Not
Terraform: the template lives with the work environment and is written on a host
that can reach it. This is the reference it is written against, and everything here
is what `tracon check-boundary` verifies on the live pod.

The current template does not qualify (one privileged Envbuilder container carrying
the Coder agent and the harness together; `docs/ARCHITECTURE.md`, "Privilege
boundary"). The replacement is the topology in `deploy/kubernetes/base`, proven on the
homelab cluster, with the Coder agent riding in the node's pod.

## The pod the template creates

Start from `deploy/kubernetes/base/deployment.yaml` and keep:

- `securityContext`: `runAsNonRoot`, uid/gid/fsGroup 65532, seccomp `RuntimeDefault`,
  `allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]`. **No `privileged`, no
  `sudo`, no root.** The `harness_unprivileged` check reads the admitted spec of the
  harness pods this node creates, but a privileged node pod defeats the point before
  any check runs.
- `serviceAccountName: tracon-node`, bound to the Role in `role.yaml` — exactly those
  verbs. The `runtime` check performs a `SelfSubjectAccessReview` per verb and fails on
  the first one missing. The Role grants nothing on NetworkPolicies beyond `get`, so
  the node cannot widen its own boundary.
- The downward-API environment: `TRACON_NAMESPACE`, `TRACON_POD_IP`, `TRACON_NODE_NAME`.
  Without them the node does not know where to place harness pods or what `tracon-gw`
  should resolve to.
- The label `tracon.dev/role: node` on the pod. Both NetworkPolicies select on it.
- `XDG_STATE_HOME=/state` with the `tracon-state` claim mounted at `/state`;
  `XDG_CONFIG_HOME=/config` with the ConfigMap's `node.toml` at `/config/tracon`. The
  claim is ReadWriteOnce; harness pods are pinned to the same Kubernetes node.
- The image `ghcr.io/cosmicspork/tracon-node:<version>`, running `tracon serve`. The
  Coder agent can run alongside it in the same container (its startup script starts
  the node and the agent), or as a sidecar; either way the container it lives in is
  the unprivileged node container, not a harness.

## What the namespace must carry

Apply `deploy/kubernetes/base` (or its equivalent in the template's Terraform):
ServiceAccount, Role, RoleBinding, the PVC, the two NetworkPolicies, the ConfigMap.
The node **verifies** these; it does not create them. In particular:

- `networkpolicy-harness.yaml` is the boundary: harness pods accept nothing and reach
  only `tracon.dev/role=node` on 7421 and 8888. The `network_isolated` check refuses a
  policy that is missing, governs only one direction, admits any ingress, or names any
  other peer, port, `ipBlock`, or `namespaceSelector`.
- The cluster's CNI must enforce NetworkPolicy. Cilium and Calico do; a CNI that
  ignores the resource passes the static check and fails `--deep`, which is why the
  deep check exists.
- Pod Security Admission at `restricted` is compatible with the harness pod spec and
  is recommended for the namespace.

## The credentials

`credentials.toml` under `/state/tracon/` (mode 600, uid 65532). `consulta` and
`jira` bound to the work channel and pinned to this node's id (`nodes = [...]`);
`glab` bound to the work channel. Model credentials are the harness's own, imported
once into `/state/tracon/harness-state/agent/agent.db` with
`tracon harness import-credentials` run inside the node pod against a copied store.

## The autostop

Coder stops the workspace after 8 hours of inactivity. The node is idempotent on
restart (`reconcile_after_restart` closes sessions that were live and deletes their
pods); sessions do not survive a stop. Background work that must outlive the
workspace is a Phase 6 concern (supervision by the platform), not this template's.

## Proving it

On the live pod, in order:

```sh
kubectl -n <ns> exec deploy/tracon-node -- tracon check-boundary --deep
```

All five checks pass, or the node refuses to run harnesses and says which failed.
Then a session from the interface (port-forward or the Coder tunnel), and from inside
its harness pod `curl https://gitlab.example` fails while `mr_status` through the
node succeeds. Record the outcome in `docs/reference/phase-3-notes.md`.
