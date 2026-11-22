# Running a node elsewhere

The desktop app and [the five-minute start](../../README.md#five-minutes-to-a-running-node)
cover a laptop. This guide covers a node in Kubernetes, on a Mac with Apple
Containerization, and built from source, and what a node does when a sandbox's
connection drops or the node restarts.

## As a Kubernetes pod

The node is an image rather than a binary and the boundary is Kubernetes: every
harness, check and preparation runs in a sandbox pod of its own over the sandbox
protocol, as on Podman, and one NetworkPolicy makes the node pod's sandbox port that
pod's only route. Manifests in `deploy/kubernetes/base`, ready for a kustomize overlay:

```sh
kubectl apply -k deploy/kubernetes/base
kubectl -n <namespace> exec deploy/tracon-node -- tracon check-boundary --deep
kubectl -n <namespace> port-forward deploy/tracon-node 7420:7420
```

The ConfigMap only seeds `node.toml`. An init container copies it onto the state
volume once, and from then on the node reads and writes the copy there
(`/state/config/tracon/node.toml`); delete that file to reseed. A change that waits
for a restart, or a restart to clear the node's state, is done with **Restart** in the
interface (the banner, or Settings › System › Restart), or `POST /api/admin/restart`:
the node drains, exits 0, and the kubelet starts the
container again on the same volume and pod IP, so a phone with a passkey restarts it
without `kubectl`. That needs `TRACON_RESTART_BY_EXIT=1` in the pod's environment, as
`deploy/kubernetes/base/deployment.yaml` sets it; without it the restart is refused. The
image carries pinned, checksum-verified releases of `gh`, `glab` and `buildctl` on its
`PATH`, so publishing to GitHub or GitLab and building repository images need no binary
mounted, and the `[publish]` and builder defaults find them. Every sandbox pod
names its own requests and memory limit (`[runtime.kubernetes.resources]`, sized by
default for one shared 4 vCPU / 8 GB node), so a namespace's LimitRange never
substitutes its defaults. Each pod also gets a memory-backed `/dev/shm` for browsers
and an emptyDir at `/tmp` that `TMPDIR` names.

- **Bundle.** The node's own: `[sandbox] bundle`, else its release's, which it fetches
  and checks into `<state>/bundles` as a Podman node does (at the
  boundary check, so at start). It serves it as one tar on the sandbox port. Each pod's
  init container, in the pod's own image (it needs `sh`, `curl` or `wget`, `sha256sum`
  and `tar`), fetches it, checks it against the digest the node wrote into the pod
  spec, and unpacks it into an emptyDir mounted read-only at `/opt/tracon`.
  `tracon-sandbox` from it is the pod's only process, and the harness it starts over
  the protocol is the bundle's. Pods run as uid 65532 with home `/home/harness`, the
  stock base's own user. The home is the image's, in the container's writable layer, as
  on Podman: only the harness's state directory (`/home/harness/.claude`) is an emptyDir
  of the pod's, so what a recipe installs under the home (`~/.cache/ms-playwright`,
  `~/.local`) is there in the sandbox.
- **Services.** A session's sandbox runs each `[[service]]` with a bundle under its
  supervisor, on its loopback, as on Podman and Apple. The node serves each bundle
  (`bundle = "release"`'s browser, which it fetches into `<state>/bundles` as it
  converges, or the operator's directory) as a tar on the sandbox port, and a session's
  pod copies it in as it does the harness bundle: one init container per service
  (`svc-<name>`), in the pod's own image, checking it against the digest in the pod spec
  and unpacking it into an emptyDir mounted read-only at `/opt/tracon-services/<name>`.
  Only a session's pod copies them, and every session's does, whether or not it starts
  one: the browser is about 280 MB of the pod's ephemeral storage and a few seconds of
  its start. A browser's shared memory is the pod's `/dev/shm` (`[runtime.kubernetes]
  shm_size`, 1 GiB by default), counted against the session's memory limit (4 GiB by
  default).
- **Files.** Nothing of the state claim is mounted, so the claim is the node's alone.
  Every directory a run names is an emptyDir of the pod's own, filled over the
  protocol and, for a session's workspace and own harness state, taken back at each
  turn end, every 60 s in a turn and at exit, as on Podman. A
  session's harness state is its own; the state every session once shared is never
  sent. Caches are sent in and come back only from a preparation: from a prepared
  snapshot, a candidate's run cache is sent over the image's own warm `/cache` and
  only what its preparation added comes back.
  How the take-backs go is the session's to see, on every runtime: the first that
  fails or times out (120 s), the one that ends a run of failures and the last one
  are `workspace_sync` events in its log; its page says how old the node's copy is
  (`files_sync` on `GET /api/sessions/:id`) and turns red when syncs fail or a turn
  has run past about 5 minutes without one; a third failure in a row while the
  sandbox runs sends one push. The node's copy is the claim's
  `tracon-workspace-<id>`. The session row's `worktree_path`
  (`tracon/workspaces/<id>`) is only the node's last export of it, for a review, a
  diff or a publication, and can be hours old.
- **Placement.** Pods sit beside this node's own pod (`tracon.dev/role=node` and
  `tracon.dev/node=<prefix>`, its `[boundary] resource_prefix`) by pod affinity on
  `kubernetes.io/hostname`, through the scheduler, unless a sandbox pool is set (below).
  The node's pod must carry both labels
  (the base Deployment sets `tracon.dev/node: tracon`), which the `runtime` check reads
  back from its own pod (`TRACON_POD_NAME`).
- **Sandbox pool.** `[runtime.kubernetes.pool]` puts every sandbox pod (a session's, a
  check's, a preparation's, the boundary's probe) on a pool of nodes instead: its
  `node_selector` labels, plus `kubernetes.io/arch` for the architecture the node
  pinned the base for, with its `tolerations`, and no affinity to the node pod. One
  pool takes sessions and checks alike. Everything a sandbox reaches crosses nodes by
  pod address and label selector: the init containers fetch the bundle over HTTP from
  the node pod's address, the supervisor dials the same address, the node takes a
  connection only from the address of the pod its token was issued to, and both
  NetworkPolicies select pods by label, not by machine. A pool the cluster's autoscaler
  grows from zero is waited for: while the pool has no node, it has room
  (`capacity.awaits_node` in the placement event); a pod the scheduler has no place for
  whose events say the autoscaler is adding a node (`TriggeredScaleUp`) waits up to
  `scale_up_grace_secs` (600) for it, and a session's start waits that much longer than
  its usual 90 seconds; one the autoscaler will not place (`NotTriggerScaleUp`: the
  pool is at its largest) is given up after `unschedulable_grace_secs` as before, the
  scheduler's and the autoscaler's words in its refusal, so a check stops short of room
  and joins the line of checks waiting for it. `start_timeout_secs` still bounds every
  start, and a check's own timeout counts the wait. The `runtime` check refuses a pool
  whose selector or nodes name another architecture than the base's, and needs `list`
  on `events` (in the base's `role.yaml`) and the cluster-wide read below.
- **Room and overflow.** With `[placement] digitalocean` on (or a requirement set), each
  starting session asks the API how much room the node pod's machine has
  (`TRACON_NODE_NAME`; affinity keeps every sandbox there), or the roomiest node of the
  sandbox pool where one is set (a node still joining counted for what it will have):
  its allocatable CPU and
  memory, less the requests of every pod bound to it that has not ended, in any
  namespace. A cordoned machine, or one with a `NoSchedule` or `NoExecute` taint, has
  none. The runtime is **full** when a session's pod and one check's pod
  (`[runtime.kubernetes.resources.session]` plus `[...resources.command]`, as
  configured) do not fit together: that reserve keeps room for the session's checks.
  `[placement] local_limit` stays an extra cap, and whichever is reached first makes it
  full, so `overflow` moves the session to DigitalOcean with no count to tune. The read
  is kept for 3 seconds, so a burst of starts reads it once; two sessions that start
  together can both fit on paper, which the grace below catches. The placement event's
  `capacity` carries `free_cpu` and `needs_cpu` (millicores), `free_memory` and
  `needs_memory` (bytes) and `fits`, and the session log says why ("runs on DigitalOcean
  · the cluster has 630 Mi free, a session and its checks need 1.5 Gi"); Settings shows
  the same beside the runtime's capabilities. It needs read-only `get` and `list` on
  `nodes` and `pods` cluster-wide (`clusterrole.yaml` and `clusterrolebinding.yaml` in
  the base; an overlay that sets `namespace:` carries the binding's subject with it).
  Without them it places by session count alone, as before, and each placement event
  says so (`capacity.unread`).
- **A pod that cannot be scheduled.** While any sandbox pod starts, the node reads its
  `PodScheduled` condition. One the scheduler keeps `Unschedulable` (no room, a taint, a
  cordon) for `[runtime.kubernetes] unschedulable_grace_secs` (60 by default) is given
  up: a session's pod, before its harness starts, is deleted and the session placed
  again with this runtime counted full, so it moves to DigitalOcean under `overflow` or
  `prefer` (or the operator is asked, as placement asks), and otherwise ends with the
  scheduler's message (`client` work, a repository that forbids DigitalOcean,
  `digitalocean = "off"`); the placement event records why. A check's or a
  preparation's pod fails its run with "could not be scheduled: 0/1 nodes are
  available: 1 Insufficient memory ...", the check's result, instead of waiting until
  its timeout, and the review's checks are placed again (below). A session's start is
  bounded at 90 seconds, so a grace longer than that ends the start before it is placed
  again. A pod the kubelet itself refuses (`OutOfcpu`, ...) is refused at once
  (`out_of_cpu`, `out_of_memory`, ...).
- **A review's checks.** They run on the node's own runtime while it has room. Once it
  has none (a check's pod could not be scheduled, was refused or evicted, or other
  checks already wait in the node's line), they are placed as a session is: with
  `[placement] digitalocean` at `overflow` or `prefer`, a repository that is not
  `client` work and does not forbid it has them run on DigitalOcean, and the rest wait
  in the line. There they start from the template of the image they run in (the
  repository's prepared snapshot, whose `/work` and `/cache` it holds), waiting up to
  ten minutes for it to build, else running here after all; only the candidate's tree
  is sent. `prepare` runs once in one sandbox with its egress grant, which is revoked
  before one sandbox per check is forked from it (four to a fork call) with no
  network of its own. Their evidence is recorded as derived from the image, and only
  derived evidence is reused there; `review_status` says where they run, and the
  session log records each placement. Nothing of a Podman node's cache volumes moves:
  the template carries its own. Templates the node takes from a peer (`templates =
  "peer"`) or one image of the operator's (`[digitalocean] image`) serve sessions alone,
  so checks there stay on the node.
- **Route.** A sandbox dials the node pod's address on `sandbox_port` (7426) over TLS
  pinned to the node's certificate. Its policy, `tracon-sandbox`, selects this node's
  sandboxes (`tracon.dev/role=sandbox`, `tracon.dev/node=<prefix>`) and allows that
  route to this node's pod and nothing else: no DNS, no ingress, none of the node
  pod's other ports, no other node's pod. `tracon-node` admits this node's sandboxes
  on that port and nothing else from them. The MCP and model gateway and both proxies,
  the per-client egress proxy on `[gateway] egress_port` (7424) among them, are tunnels
  over the connection, and the gateway's name is the sandbox's loopback, so a harness
  addresses them as it always did.
- **No Secrets.** A run's secret environment (a session's gateway token, a lent
  `prepare_credentials` value, an egress grant's proxy URL) travels with its spawn over
  the protocol, never into a pod spec or a Secret. The run's one-time token is in the
  pod spec, where it is worth nothing once spent: the first hello that presents it
  consumes it, the node only takes a connection from the address of the pod it issued
  the token to (anything else is closed before it is read), and the token is revoked
  when the run ends or fails to connect.

The node's Role (`deploy/kubernetes/base/role.yaml`) is exactly what the `runtime`
check asserts: pods (create, get, list, delete), `pods/log` (get), and NetworkPolicies
(get, list). Nothing on Secrets and no `pods/attach`. The checks read the provider's
own rendering back after admission (a gated probe), the `tracon-sandbox` policy's
shape, that no other NetworkPolicy in the namespace selects the node's sandboxes with a
rule of its own (policies add up, so one would widen the route; the Role lists them for
this), and that `tracon-node` admits sandboxes on the port; a sandbox's
`verify_isolation` reads the same and reports `route: unverified` when another policy
widens it. `--deep` runs the egress script in a sandbox through its tunnels, asks the
per-client proxy with no credentials and expects a 407, and checks that no name
resolves and that the node pod's forward port is not reachable directly. Names follow
`[boundary] resource_prefix`: the policies a node reads are `<prefix>-sandbox` and
`<prefix>-node`, and the repository images it pushes are `<registry>/<prefix>-repo-…`.

In a cluster both of the node's proxies, the allowlist proxy on `proxy_port` and the
per-client one, refuse to dial a private address (RFC 1918, `100.64.0.0/10`,
`fc00::/7`), this machine, the link, any block in `[runtime.kubernetes] internal_cidrs`,
and names ending `.svc`, `.cluster.local` or `.internal` or with no dot, whatever a grant
or `allow_hosts` names. Each looks a name up once, as absolute, and dials only the
addresses it checked. On a cloud, list the nodes' and load balancers' public addresses
in `internal_cidrs` too: they reach the cluster from outside it, and nothing private
about them says so. An entry that is not an address block is refused when `node.toml`
loads.

**Upgrading a node that ran harness pods.** `[runtime.kubernetes] sandbox_protocol =
false` is refused when `node.toml` loads, naming the way over: apply the shipped
manifests (the Role above, `tracon-node`, `tracon-node-build` and `tracon-sandbox`),
delete the line, and move a `[[repo]]` entry's `image`, or a Dockerfile not on
`FROM ${TRACON_SANDBOX_BASE}`, to `build` steps. `sandbox_protocol = true` and the
`harness_image`, `state_claim`, `uid` and `harness_home` keys are ignored with a line in
the log, and can go. What the harness pods left is the operator's to remove: pods
labelled `tracon.dev/role=harness`, any run Secret a node that died left without an
owner (`kubectl delete secret -l tracon.dev/run`), the `tracon-harness` NetworkPolicy,
and the layered `-claude`/`-native` session images in the builder's registry.

**DigitalOcean from a Kubernetes node.** A pod-hosted node over the sandbox protocol can
place its sessions on DigitalOcean's Harness Runtime and build templates for itself and
its peers, as a Podman node does. Its trees are sent from the state claim and its
gateways are the same tunnels its own sandboxes get. Set `[digitalocean] credential`,
`registry` (equal to `[runtime.kubernetes.builder] registry`: BuildKit builds each
template's source on the cluster and pushes it with the builder's credential), and reach
the node through the hub relay: `relay = true`, `listen = "127.0.0.1:<port>"` on a port
other than the sandbox port (7426), and `[mesh.sandbox_relay] target` the same address.
The node pod's own egress is open, so it reaches `api.digitalocean.com` and the hub with
no policy change. The node must run on amd64, which is all the runtime runs, and the
release's browser bundle it fetches into `<state>/bundles` is what the templates hold.
Prepared snapshots stay in the builder's registry; the store's ready ones keep their
templates from the sweep.

**Two nodes in one namespace.** Give the second its own `resource_prefix` (say
`spike`) and its own Deployment, whose selector and pod template carry
`tracon.dev/node: spike` (the first's keeps `tracon`), and copies of the base's
policies named `spike-sandbox` and `spike-node` with `spike` in every
`tracon.dev/node`. Every pod a node makes carries its prefix in that label and its
name; each node counts, sweeps and places only its own, and the checks refuse a
sandbox policy that selects or reaches another node's pods.

**Probe before the node serves.** `tracon sandbox probe --provider kubernetes`, run in
the node's pod (`kubectl exec deploy/tracon-node -- tracon sandbox probe --provider
kubernetes`), makes one sandbox pod as the node would (the stock base unless
`--image`, this release's bundle unless `--bundle`) under a name of its own
(`<prefix>-probe-…`; `--name` is refused here, since a pod of the given name would be
replaced), takes its connection on the sandbox port, runs a command in it, reads its
isolation back (the admitted pod, `<prefix>-sandbox` and the namespace's other
policies) and removes it, on Ctrl-C too. It needs the sandbox port free, so it is for a
pod whose node is not serving yet; once it is, the boundary check covers the same
ground: `tracon check-boundary [--deep]` in the node's pod finds the sandbox port taken
and asks the serving node on its loopback API (`POST /api/boundary/check`, with
`{"deep": true}` for `--deep`) to run the checks, and prints its answer and exits as a
check run in the pod's own process would.

### Images on Kubernetes

There are no harness images: every sandbox runs the stock base (`[sandbox]
base_image`), or a repository's image built on it from its `build` steps (or a
Dockerfile on `FROM ${TRACON_SANDBOX_BASE}`), with the harness from the bundle, as on
Podman. The node pins the base when it converges (at start, and whenever the release,
`base_image` or `bundle` changes) by the manifest its registry holds for the node's
own architecture, which is every sandbox's, since affinity keeps them on the node pod's
machine; nothing is pulled by the node, and a pod's kubelet pulls the pinned digest.

A pod-hosted node builds a repository's image only when `[runtime.kubernetes.builder]`
names a BuildKit daemon and a registry. Without it nothing is built or snapshotted: an
entry with `build` steps has no image (the session is told why), every session runs its
own `prepare`, and the `runtime` check says the builder is not configured. The node
image carries `buildctl`; the daemon is the deployment's own (a rootless
`moby/buildkit` in a namespace of its own, for example), listening for mutual TLS. Each
build sends its context from the node (`--local`), pushes the result (`push=true`), and
pins the digest the daemon reports. The node names images as Podman does,
`localhost/tracon-repo-…`, and that prefix becomes the configured `registry` here:
`localhost/tracon-repo-app-1a2b3c4d5e:0123` is pushed as
`<registry>/tracon-repo-app-1a2b3c4d5e:0123`.

A repository with a `prepare` is prepared once on its default branch in a sandbox pod
from that image and snapshotted. A pod cannot be committed, so its `/cache` and `/work`
are read back over the protocol (as a preparation's tree always is), layered onto the
image it ran (`COPY --chown=65532:65532`, which keeps the image's user, home and
environment) and pushed as `<registry>/<prefix>-snapshot-…`. Sessions and checks start
from it, and a session at the snapshot's commit skips `prepare`, as on the other
providers.

Build steps reach the network through the node: the node pod admits the daemon on its
egress port (the base's `tracon-node-build` policy does, for a daemon in
`tracon-build` labelled `app.kubernetes.io/name: buildkitd`; change its selectors if
yours is elsewhere), and the daemon's own egress policy must allow it there. Whether a
step can go around the proxy is the daemon's policy's to decide. One that allows DNS,
the node pod's egress port and the registry it pushes to (and pulls the base from) and
nothing else holds every step to its `egress`, as Podman and Apple do; one that reaches
the internet, as a daemon that builds Dockerfiles needs, holds only the tools that
honour a proxy. To pull the base without the internet, keep a copy of it in the
builder's registry and name that in `[sandbox] base_image`.

What the node pod needs:

- **The client certificate.** A Secret holding `ca.crt`, `tls.crt` and `tls.key` (what
  cert-manager issues), mounted at `tls_dir`. The daemon's certificate must name the
  host in `addr`.
- **The push credential.** A Docker `config.json` with a credential that may push to
  `registry`, mounted in the directory `docker_config` names (a
  `kubernetes.io/dockerconfigjson` Secret with its `.dockerconfigjson` key mounted as
  `config.json`). `buildctl` reads it as `DOCKER_CONFIG`, and the node reads the same
  file to look images up, pin a base kept in that registry, and remove superseded ones.
  Credential helpers are not consulted. The credential is mounted only in the node's
  pod, but it is not kept from the daemon: during a build, BuildKit's session auth
  answers the daemon's credential requests with this username and secret, so a daemon
  running a hostile Dockerfile can obtain it. Give it a credential scoped to the
  registry and nothing else. DigitalOcean's `doctl registry docker-config --read-write`
  is one: its token is refused by the rest of the DigitalOcean API (account, Droplets,
  Kubernetes all answer 403) and accepted only for the registry.
- **Egress.** TCP to the daemon's port, and HTTPS to every registry a build pulls from
  or pushes to *and to their token services*, because the client fetches the tokens:
  `ghcr.io` for the release's base and bundle, `registry.digitalocean.com` and
  `api.digitalocean.com` for DigitalOcean's registry. This is the node pod's own
  traffic, not a sandbox's: it does not go through either of the node's proxies, so
  their refusal of private addresses and `internal_cidrs` does not apply to it, and the
  node pod's own egress policy has to allow it.
- **Room for a build context.** A build stages its context under `<state_mount>/tmp`
  on the state volume, and runs `buildctl` with `TMPDIR` there, so a large repository
  does not fill the node container's ephemeral storage. (A sandbox pod's `TMPDIR` is
  its own `/tmp` emptyDir.)
- **Pull access for sandbox pods.** They pull the built images by digest with the
  namespace's pull secrets (`[runtime.kubernetes] image_pull_secrets`, else the
  `default` service account's); DigitalOcean's cluster integration adds them to the
  `default` service account.

The `runtime` check runs `buildctl debug workers` over mutual TLS, then asks the
registry's token service for push on `<registry>/tracon-repo-probe` and presents the
token to `/v2/`, and appends what it found to its detail. Only an explicit refusal
fails it: a token service that refuses the credential, or a token that names that
repository without push. A token that does not say is accepted, and the detail reads
"push not confirmed". The whole check is bounded at 15 seconds. A table that cannot be a
builder, or a builder that fails either probe, is a warning there and never refuses
the node: sessions and checks still run. A build attempted anyway fails with the same
reason the check gave, and that failure is not held against the repository's recipe.
The recipe is left alone for two minutes, so the check runs queued behind an outage
do not each try again, and the next check or preparation after that tries again
without `tracon repo build`, which retries at once. A session that finds the builder
down starts on the stock base with the reason, rather than waiting. A registry that
cannot be asked whether an image is still there leaves the standing image in use and
triggers no rebuild.

The node removes only what it pushed, `<registry>/tracon-repo-…` and
`<registry>/<prefix>-snapshot-…`, never an image an entry pinned. A superseded image is kept for one more generation, because a pod
already scheduled on it may not have pulled it yet: the build two back is removed when
a new one finishes. Removal is by digest through the distribution API. DigitalOcean's
registry routes that delete but may refuse it, so on that host the node falls back to
the DigitalOcean API (`DELETE /v2/registries/<registry>/repositories/<repo>/digests/<digest>`)
with `do_token_file`, or the credential's password. Deleting a manifest frees no space
until the registry's garbage collection runs; schedule it with
`doctl registry garbage-collection start --include-untagged-manifests`, so the
untagged manifests a rebuild under the same tag leaves behind go too.

## On a Mac with Apple Containerization

On Apple silicon with macOS 26, `[runtime] kind = "apple"` runs every harness, check and
preparation in a lightweight VM of its own, made with Apple's
[`container`](https://github.com/apple/container) CLI. It is a Mac's default and the only
kind a Mac runs; Podman is not supported there, and `kind = "podman"` is refused at load.
Install `container`'s signed package, then run
`container system start --enable-kernel-install` once. Without that flag, starting it
prompts for a kernel and fails when there is no TTY. The desktop app's setup page checks
both, and installs the service only once `container system status` says its services run.

A Mac that ran Podman before needs nothing in `node.toml` beyond removing any
`kind = "podman"`: its next start runs on `container`. Sessions opened under Podman stay on
the Podman machine's volumes, which the node no longer reads; finish or export them first.
The Podman machine itself is left alone (`podman machine rm` when nothing else uses it).

- **Route.** Each VM has `--network none` and no resolver. Its only way out is the
  sandbox protocol's socket, which `container` publishes from the VM to a socket under
  `/tmp/tracon-<uid>`. The MCP and model gateway, the allowlist proxy and the per-client
  egress proxy are tunnels on the VM's loopback, and the harness reaches them at
  `127.0.0.1` because `container` cannot add a hosts entry.
- **Files.** Every volume is a tree in the node's `<state>/sandbox-files`. It is sent into
  a sandbox when the sandbox starts and taken back as on a Kubernetes node.
- **Images.** Harnesses come from the bundle (`[sandbox] bundle`, else the release's
  arm64 bundle), mounted read-only at `/opt/tracon`. They run on the stock base
  (`[sandbox] base_image`), which is pulled and pinned at start by its linux/arm64
  manifest (only that platform is pulled), or on a repository's
  image, which `container build` makes in `container`'s own builder VM and keeps in its
  image store. The builder VM holds 2 CPUs and 2 GB while it runs, so the builds that
  started it stop it when the last of them is done; a builder that was already running is
  left alone. Sandboxes run as the base's user 65532 with home `/home/harness`.
- **Prepared snapshots.** A repository's `prepare` runs once on its default branch in one
  sandbox, which is then committed (`container commit`) as
  `localhost/<prefix>-snapshot-<repo>`. Its sessions and checks start from that image.
  The commit keeps the preparation's environment, so the node builds it over once more
  with the base's configuration, the preparation's token emptied and the snapshot's
  labels, among them the commit it was prepared from. A session at that commit skips
  `prepare`. A commit is one flattened layer that holds the repository's toolchain and
  what `prepare` cached, so a snapshot costs far more on disk than the base: tracon's is
  about 2.7 GB. `tracon gc` removes snapshots that a newer one superseded, with the
  other image classes (see [Starting work](sessions.md#starting-work)).
- **Placement.** The node reports its sandboxes as tier `microvm`, so work that
  requires one (`tier = ["microvm"]`) stays on the Mac.

The checks start one sandbox, run `id -u` and a write to the bundle in it, and read its
isolation back from `container inspect`: no network, only the node's socket published,
no SSH agent forwarding, no nested virtualisation, and the bundle mounted read-only.
`--deep` runs the egress script in a sandbox through its tunnels.

## From source

Rust, Bun, and rootless Podman:

```sh
just build      # the SPA, then the release binary (target/release/tracon)
just setup && just boundary
./target/release/tracon serve
```

`just musl` builds the static Linux binary; `just gui` builds the desktop app in a
container (it needs webkit2gtk headers an immutable host lacks). The interface's
screenshots regenerate without a node: `cd spa && bun scripts/screenshots.mjs`. Every
screen in every state worth checking, at phone and desktop widths in both themes, comes
from `cd spa && bun scripts/ui-audit.mjs`, which writes a contact sheet to
`target/ui-audit/index.html`.

To run an unreleased build as your own node, against your real state and credentials, use
`just node-dev [ref]` (default `origin/main`). It builds the ref in a worktree under
`~/.cache/tracon/node-dev` and refuses while a managed session is running. It then stops
the node and copies its state and config directories to `tracon-backups/` beside the state
directory (`~/.local/state` on Linux, `~/Library/Application Support` on macOS), then runs
the build's `setup`. Finally it starts the service on `tracon-dev` beside the installed
binary and runs `check-boundary --deep`. On Linux that is a systemd drop-in; on macOS it
rewrites the LaunchAgent's `ProgramArguments` and keeps the release's plist as
`tracon-backups/release.plist`. Running it again replaces the build and keeps the first
backup. `just node-release` removes the drop-in or puts the plist back, rebuilds the
boundary from the release's own definitions and starts the release again. `just
node-release restore` also puts back the backed-up state, keeping what the dev build wrote
beside it.

The build runs as `tracon-dev` instead of replacing `~/.local/bin/tracon` because the
desktop app owns that path and reinstalls it whenever its version differs. Migrations run
forward only, which is why the backup is taken. Podman volumes and images the dev build
created are not backed up or removed.

### A scratch node

To try a build without touching the node you run, start a second one beside it with its
own state, configuration, port and runtime names. Everything a node creates in Podman is
named from `node.toml`, so a scratch node with its own values shares none of the real
one's harness state, images, probes or sockets, and neither node's `tracon gc` lists the
other's volumes. Its sandboxes have no network and no gateway of their own, so a prefix
of its own is all the boundary needs.

```sh
# Not under the temp directory: sessions' worktrees go there, and it may not hold the config.
export TRACON_STATE_DIR=$(mktemp -d -p ~/.cache) TRACON_CONFIG_DIR=$(mktemp -d -p ~/.cache)
export TRACON_LISTEN=127.0.0.1:7520 TRACON_URL=http://127.0.0.1:7520  # serve, and the CLI's calls
cat > "$TRACON_CONFIG_DIR/node.toml" <<TOML
[boundary]
resource_prefix = "scratch"        # volumes and containers: scratch-harness-state, scratch-h-…

[gateway]
harness_listen = "$XDG_RUNTIME_DIR/tracon-scratch/harness.sock"

[docs]
preview_listen = "127.0.0.1:7522"
TOML
./target/release/tracon setup && ./target/release/tracon check-boundary --deep
./target/release/tracon serve
```

Its repository images and snapshots are named `localhost/scratch-repo-…` and
`localhost/scratch-snapshot-…`, so neither node moves the other's tags, and each carries
the node's owner labels, so neither node's `tracon gc` removes the other's (a node with
the default prefix keeps `localhost/tracon-repo-…`). To clean up, stop the node, then
remove what carries its names, and its two directories:

```sh
podman ps -a --format '{{.Names}}' | grep '^scratch-' | xargs -r podman rm -f
podman volume ls -q --filter name='^scratch-' | xargs -r podman volume rm
podman images --format '{{.Repository}}:{{.Tag}}' | grep '^localhost/scratch-' | xargs -r podman image rm
rm -rf "$TRACON_STATE_DIR" "$TRACON_CONFIG_DIR"
```

## When a sandbox's connection drops

A run over the sandbox protocol (Podman, Kubernetes, Apple, DigitalOcean) outlives a
dropped connection:

- **The node's identity is kept.** The serving node keeps the certificate its sandboxes
  pin in `<state>/sandbox-identity/` (`cert.der` and `key.der`, 0600), made on the
  first start that needs it. To rotate it, stop the node and remove the directory: the
  next start makes another, and every sandbox started before it refuses the new
  certificate and ends its run (its processes are stopped and its trees were last taken
  back at the last sync).
- **The sandbox comes back.** With its first ack the node gives the sandbox a resume
  secret, inside TLS, bound to that run (and on Kubernetes, as the token is, to the
  pod's address). When the connection drops, everything in the sandbox keeps running
  and the sandbox redials with the secret, with backoff, for twice
  `[sandbox] resume_within_secs`; on Apple it listens again and the node connects in
  again. The node waits `resume_within_secs` (default 120) before the run is lost and
  ended as before. The secret is revoked when the run ends, and a sandbox the node
  refuses (the run is over, or the certificate is not the one it pinned) tears itself
  down at once.
- **Nothing is lost or repeated.** Output written while the node is away waits in the
  sandbox, up to 4 MiB a stream; past that the process blocks writing, as on a pipe
  nobody reads. The node asks again from the byte it had reached. Stdin is sent again
  from the last 2 MiB the node keeps, and the sandbox skips what it had written. An
  exit while the node is away is told when it is back. A service's log lines and
  state changes from the gap are not resent; its state can be asked for.
- **Compatibility.** The bundle is the node's own version. A sandbox binary from before
  this (no `resumable` in its hello) is given no secret, and its run ends with its first
  connection, as it always did.

### When the node restarts

A stop and a restart are the same to the node: on SIGTERM it ends its live sessions
(`node_restart`) and takes their trees back as at any end, but leaves each session's
sandbox that can resume running, with its services and its tunnels' listeners, and
records it in `<state>/sandbox-runs/` (its run, the provider's handle, the digest of
its resume secret, its harness process, whether a turn was running; never the secret).
Its harness goes as the node closes its stdin.

On the next start the node carries each of those sessions on, on the same row: it
reopens the session's own conversation (as **Ask** does), and the runner takes the
sandbox back instead of making one. It knows the run again by the digest, sets the
provider's carrier up again (Podman binds its socket again in the directory the
container mounts, Apple connects in, Kubernetes expects the pod's address again,
DigitalOcean's listener takes it as it is), waits for the sandbox to redial, and starts
the harness in it, on the workspace and harness state the sandbox kept. A session that
was in a turn is told the node restarted during it and to carry on.

A sandbox the next start does not take back goes: one whose session cannot be reopened,
one that does not redial within `resume_within_secs`, or one the provider reports gone
leaves its session closed as `node_restart`, to be continued as before. A node that does
not come back is never waited for: each sandbox ends itself once its redials run out
(twice `resume_within_secs`), and the next start removes what is left. A node started
with `resume_within_secs = 0` leaves nothing running when it stops.
