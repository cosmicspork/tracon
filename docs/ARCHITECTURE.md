# Architecture

Design record for the agent orchestration system. Decisions here are settled unless
listed under [Open questions](#open-questions). Rationale is included where the
reasoning is not obvious from the decision.

`tracon`: named for terminal radar approach control. The facility sequences aircraft and issues clearances; it never flies one. The node supervises and gates; it never reasons.

## Problem

Agent work is currently driven from TUIs on two laptops. That produces four distinct
problems:

1. **Latency.** Work-side agents run against Coder workspaces on another continent
   from the operator. A TUI round-trips every keystroke. A chat interface round-trips
   once per prompt.
2. **Environment sprawl.** Per-application Coder workspaces are managed individually,
   and agents running locally against remote environments force devcontainers on the
   laptop just to run tests.
3. **No enforcement.** Working agreements (worktree not main checkout, review before
   publish, no merge, no production deploy) are prose at the top of a file the agent may
   not still have in context an hour into a session.
4. **No visibility when laptops are closed.** Nothing is observable or controllable from
   a phone.

The system is a supervisor and control plane that sits between the human and existing
coding agents. It does not implement an agent loop.

## Principles

- **The node supervises, it does not reason.** No model loop in the node. Harnesses are
  drop-in and replaceable; anything that tracks harness features will rot.
- **Enforcement over instruction.** A rule the node cannot enforce is a suggestion.
  Where enforcement is impossible, say so explicitly rather than implying uniformity.
- **Local-first.** A hub outage degrades the system. It does not stop work.
- **Nothing in project repos.** No `.claude/`, no `AGENTS.md`, no `.beads/`. Config is
  materialized into scratch directories at session start.
- **The corpus outlives the tooling.** Memory, docs, work items, and events use boring
  schemas with plain-text export. When the orchestration layer becomes obsolete, the
  accumulated context must survive it.

## Topology

```
                        ┌──────────────────────┐
                        │   hub (cluster)      │
                        │   always-on          │
                        │   relay: routes      │
                        │   ciphertext per     │
                        │   channel            │
                        └──────────┬───────────┘
                                   │
                       ┌───────────┴───────────┐
                 ┌─────▼──────────┐   ┌────────▼───────┐
                 │ eligible nodes │   │ PWA / desktop  │
                 │ any host that  │   │ clients        │
                 │ passes checks  │   │                │
                 └─────┬──────────┘   └────────────────┘
                       │ supervised exec
                 ┌─────▼──────────┐
                 │ isolated       │
                 │ harness runner │
                 └────────────────┘
```

Every admitted node dials out to the hub. Nothing accepts inbound connections. Host
location does not determine admission: a laptop, Coder workspace, or managed environment
is a node only when it can establish and verify the harness boundary.

**The hub is the relay.** An earlier draft had a separate opaque relay in front of the
hub. Both must be always-on, both would live in the same cluster, and the hub already
has to treat work channels as ciphertext-only forwarding, which is what a relay is.
Merging removes one deploy and one availability dependency. What is preserved from the
split: the work channel stays opaque to the hub, the policy signing key is never on
it, and channel-scoped keys mean the relay role can be split back out later if the
hub's host ever becomes the less trusted option. See [The hub](#the-hub).

## The node

A single statically linked Rust binary. Responsibilities:

- Supervise harness processes and own their lifecycle
- Speak the mesh protocol to the hub
- Serve the embedded SPA
- Evaluate policy locally
- Broker action credentials, and expose brokered tools over MCP (see
  [Brokered tools](#brokered-tools))
- Persist sessions, events, memory, docs, and work items to local SQLite

Explicitly **not** responsibilities: model inference, an agent loop, prompt
construction beyond assembling injected context, business domain (see
[Personal accounting is out of scope](#personal-accounting-is-out-of-scope)).

### Build constraints

- Static musl build. The Coder base image's glibc is not under our control.
- `x86_64` and `aarch64`.
- SPA embedded via `rust-embed`. Every node serves the identical bundle, so "local
  interface" and "remote interface" differ only by origin.
- Log to stdout, no self-daemonizing, clean SIGTERM, idempotent restart. Supervision is
  external (see [Clients](#clients)).
- Static musl rules out anything that needs Oracle Instant Client (`libclntsh` is a
  dynamically linked glibc blob, and every Rust Oracle crate wraps it). This is why
  consulta stays a Python sidecar rather than being rewritten: `oracledb` thin mode is
  a pure-Python wire implementation with no native client. Recorded so the rewrite is
  not re-decided.

### Distribution

Fetched as a binary by a one-line bootstrap. **Not** shipped with the operator's shell
configuration. That is configured for interactive human use and is heavy; agent-only
environments should not pay for it, and coupling the two means every node change
requires a change there too.

## Harness control

### Protocol

**ACP (Agent Client Protocol) is the adapter interface.** It carries session management,
tool execution, and interactive permissions over JSON-RPC on stdio, and provides
filesystem routing, terminal routing, and permission prompts without bespoke glue.

Phase 0 validated these assumptions against `omp acp` 18.0.4. The
[`session/new` and update capture](reference/acp-omp-18.0.4-session.jsonl) includes model
selection, permission requests, tool updates, and usage events. A
[`restricted-session` capture](reference/acp-omp-restricted-session.jsonl) and its
[`driver`](reference/acp-drive-restricted.py) show omp accepting denied egress and absent
publishing tools without retrying, routing around the gate, or reporting false success.

The node declares ACP filesystem reads unavailable so omp reads inside its isolated
runner rather than turning the node into a file server. `session/request_permission` is
the gate input. Usage events include cost, and budget accounting includes the large,
mostly cached startup context rather than counting only the visible prompt.

| Harness | Mode | Notes |
|---|---|---|
| omp | `omp acp` | Also exposes `_omp/*` for session discovery and reopen. Non-spec. |
| opencode | `opencode acp` | Brings up an HTTP server; see cautions below. |
| Claude Code | native | `--input-format stream-json --output-format stream-json --permission-prompt-tool stdio`, one resident process, messages on stdin. |

Claude Code is the special case. Its `control_request` / `control_response` path is
functionally equivalent to ACP permission requests but is not ACP.

The adapter trait exists from the first commit; only the omp adapter was built until a
concrete task needed another. Adapters are the part of the system that rots, and three
of them on day one is scope.

**Built (Phase 7): the Claude Code adapter.** What the table above records as its mode
is out of date in one respect — `--permission-prompt-tool` is no longer in the CLI's
help, and a reading of the published documentation concludes that external permission
brokering needs the TypeScript SDK. It does not. The control protocol is in the shipped
binary, and the SDK is a wrapper that spawns that same binary and speaks stream-json to
it, so the adapter speaks it directly: `can_use_tool` arrives as a `control_request` and
is answered with `{behavior: "allow"}` or `{behavior: "deny", message}`. The frames are
recorded in `reference/phase-7-notes.md`, because they are in neither the help nor the
docs.

Making room for it turned the seam into one. Four things outside the trait had spelled
omp — the state directory, both runners' state variable, the harness's config files, and
the construction of the adapter itself, which meant `[harness] id` was read nowhere at
all and setting it to anything ran omp regardless. A harness now declares its own state
layout and config files, and an unknown id refuses to start rather than falling back.

Version-pin harnesses per node, record the version on every session, and check
compatibility at session start. This layer is the most likely thing to break and it will
break silently.

### Cautions

- **opencode's ACP mode starts an HTTP server** with `--port`, `--hostname`, and
  `--mdns` options; with mDNS it advertises itself on the local network. Bind loopback,
  disable mDNS. `OPENCODE_SERVER_PASSWORD` exists but has been reported to break IDE
  communication when set through ACP config.
- **`_omp/*` is non-spec.** Useful, but uniformity leaks. Budget for per-harness quirks
  even with a shared protocol.
- **Deny-by-default is correct.** In omp's ACP mode, requests requiring interactive
  permission are denied rather than silently approved when the client cannot answer.
  This is the desired behavior during a hub outage, and it is how "local-first" and
  "fail closed" coexist: policy is evaluated on the node, so auto-allowed work
  continues with the hub down, while anything needing a human approval blocks until
  one can be reached. Degraded means slower, not stopped, and never silently
  permissive.

### Model auth

**Decided 2026-08-28: model credentials become brokered, like every other credential.**
Phases 1–3 kept them in the harness's own store (`~/.omp/agent/agent.db`,
`~/.local/share/opencode/auth.json`) on a volume the node owns and mounts
(`OMP_STATE_DIR`), imported once from an operator's laptop. That was the one credential
the harness still held, and it is the one the gate cannot see: a provider binding
("work channel, local models only") is not enforceable while the harness talks to the
provider itself. It is replaced by the split below, scheduled as the first item of
Phase 4.

**The node owns the gateway, the store, and the bindings.**

- A model gateway on the internal network, the CONNECT proxy grown one step: the harness
  sends its ordinary provider request to `http://tracon-gw:<port>/<provider>/…` and the
  node injects the credential and forwards over TLS. No interception — the harness's
  own request is forwarded with its own shape, which is what subscription OAuth tokens
  (issued to a specific client) require.
- Provider credentials live in the node's sealed store with `channels` and `nodes`
  bindings like any other, are handed off over the mesh with channel keys, and never
  appear on a harness volume. `agent.db` on the volume, `harness import-credentials`,
  and `harness shell` have since been removed.
- The gateway is the enforcement point for provider bindings (a channel bound to local
  models is refused a hosted provider, fail closed) and the counting point for per-
  channel cost ceilings: every model call passes through it, so usage is measured where
  it happens rather than reported by the harness.
- API-key providers (GradientAI, OpenAI keys, a local model endpoint) are a header.

**The node does not implement the vendors' OAuth.** The subscription flows are the
harnesses' own clients — undocumented, changed without notice, a grey area to drive
from a third-party binary, and the most churn-prone surface in the system. So the node
*runs* the harness's login and refresh as owned subprocesses inside the boundary
(`omp auth-broker login`, `claude setup-token`, `opencode auth login`), surfaces the
URL and code on the Nodes screen as a "connect a provider" card (a paste-back code is
answered through the same card), and lifts the resulting token from the scratch store
into its own vault. Refresh is the same move on a timer. The vendor logic stays in the
vendor's binary; the node owns everything around it. The adapter trait gains
`login(provider)` and `refresh(provider)`, and each adapter carries the one line that
points it at the gateway (`ANTHROPIC_BASE_URL`, opencode's provider `baseURL`, omp's
`auth-gateway` setting).

This supersedes the "model-proxied credentials" entry in the roadmap's deferred list,
which objected to TLS interception; header injection on the internal network is not
that.

*Built (Phase 4):* `gateway/model.rs` on the harness listener, `[providers.<name>]`
in `node.toml` (`credential`, `upstream`, `shape`, `login`), the broker's `inject_for`
(an `api_key` becomes `x-api-key` or a bearer by shape; an `oauth` credential a bearer
with the OAuth beta flag merged), `model_usage` counted off the streamed response, and
`providers.rs` running the harness's login against `<state>/providers/<provider>/`. The
spike found omp needs `ANTHROPIC_BASE_URL` for Anthropic and a materialized
`models.json` provider override for OpenAI, and that the sign-in URL is pasted back
because the login's localhost callback is unreachable from the operator's browser.

Work-side model access is through the employer's provider subscription via
omp/opencode. Subscription access and API-platform access are separate systems at
every major provider, so there is no assumption of a programmatic API endpoint on the
work side.

## The gate

The review server becomes a real gate rather than an advisory one.

### Privilege boundary

The gate only works if the harness cannot reach what the node holds. **Every node
establishes this boundary or does not run harnesses.** There is no advisory mode.

The boundary is capability-driven, not tied to an operating system or product. A host
qualifies when it can run the long-lived node, give the node exclusive control of an
isolated harness runner, persist node and harness state, serve the SPA, and pass the
startup checks below.

The current Coder template does **not** qualify. It produces one Envbuilder-built
devcontainer that carries the Coder agent and the harness together. Its Kubernetes
container is `privileged`, the `coder` user has passwordless `sudo`, and root has the
full capability set. It has no Docker CLI or mounted Docker socket, but that is
irrelevant: the harness can become privileged root in the node's own container.

Work-side enforcement therefore requires a replacement topology: an unprivileged harness
runner separated from the node, with the node holding its exec pipe and being its only
permitted network route.

Phase 0 established one working topology on an immutable Fedora host with rootless Podman. The node owns:

- an **internal network** (`--internal --disable-dns`), which has no route to the
  internet and, in rootless Podman, none to the host either;
- a **gateway container** on both that network and the default one, running an HTTP
  CONNECT proxy with a default-deny allowlist of model provider hosts, and forwarding
  one internal port to the node's Unix socket; and
- the **harness container** on the internal network only, `--cap-drop=ALL`,
  `no-new-privileges`, `HTTPS_PROXY` pointed at the gateway, with the node holding the
  exec pipe.

The harness therefore reaches exactly two things: allowlisted provider hosts through
the proxy, and the node through the gateway. A Unix socket mounted straight into the
harness does not work under SELinux without `label=disable`, which is why the gateway
carries it instead. The verified proxy configuration is
[`reference/gateway-tinyproxy.conf`](reference/gateway-tinyproxy.conf). The hub runs no
harnesses and needs no boundary.

The proof also established operational constraints that belong in the implementation:
disable the internal network's DNS so its resolver does not answer `NXDOMAIN` for every
external name, keep the node socket under `$XDG_RUNTIME_DIR` to stay below the Unix
socket path limit, and mount the socket only into the trusted gateway. omp honored the
gateway's `HTTPS_PROXY`: the provider host was reachable and an unlisted host was denied.

#### What implementing it changed

Three things the design assumed did not survive contact, recorded here so they are not
re-decided. Evidence is in
[`reference/phase-1-spikes.md`](reference/phase-1-spikes.md).

- **The node-to-harness forward is TCP on a Podman machine, not a Unix socket.** A host
  socket cannot be bind-mounted into the VM, so the gateway's `socat` forward points at
  `host.containers.internal`. A host listener bound to loopback is reachable that way
  under `applehv`, so the node does not have to expose a non-loopback port. On a Linux
  node the Unix socket described above still applies. The forward carries a per-session
  bearer token either way.
- **The harness image ships its own harness binary.** The operator's `omp` is a darwin
  executable and cannot be mounted into a Linux runner, so the image installs the pinned
  Linux release instead. The version is checked twice against the pin: once by running
  `omp --version` in the runner, and again from `initialize.agentInfo.version`.
- **The harness state directory is node-owned, and only the credential database is
  mounted into it.** Mounting the operator's whole `~/.omp` drags in its `AGENTS.md`,
  which is a symlink into the workspace, and **a bind mount over a symlink does not mask
  it**. The node therefore builds an otherwise-empty state directory and mounts only
  `agent.db` (and its WAL and shared-memory files) plus a materialized `config.yml`. This
  is the same "nothing in project repos" commitment applied in the other direction:
  nothing of the operator's leaks into a session either.

- **On Linux the gateway forward is a Unix socket, and the gateway runs unconfined
  under SELinux.** Found in Phase 2 on the SELinux node: `host.containers.internal` is
  a pasta interface address, not loopback, so the TCP forward cannot reach a loopback
  listener; and SELinux's `connectto` forbids a confined container connecting to an
  unconfined listener's socket whatever the file is labelled. The gateway is the trusted
  piece and exists so the harness never touches the socket, so it runs `label=disable`;
  the harness keeps its confinement. The deep check asserts the forward carries
  traffic. Evidence in [`reference/phase-2-notes.md`](reference/phase-2-notes.md).

That host is evidence for the design, not a Phase 1 platform requirement. macOS with a
Podman VM, another Linux host, or a managed web environment qualifies only if it can
provide the same capabilities and pass the same checks. **macOS with a Podman machine was
the first implemented node** and qualifies: the boundary holds inside the VM, and the
implementation notes below record where it differs from the Linux proof. Claude Code for web can be used
to implement Phase 1; it is not a Phase 1 runtime unless its environment can host the
persistent node, isolated runner, state, and reachable SPA.

Three things collapse the boundary and must be verified per environment:

1. **Runtime control in the harness.** A Docker or Podman socket, Docker-in-Docker, or
   another path to the node's container runtime gives the harness control of the gate.
2. **A shared privilege domain.** The harness must be unprivileged and isolated from the
   node. Root or equivalent capabilities in the node's container fail the boundary even
   when no runtime socket is mounted.
3. **Direct egress.** The harness runner must be on an internal network with the node
   gateway as its only reachable route. Image build may use network; execution may not.

#### A pod-hosted node

The work topology is the same boundary with Kubernetes doing what rootless Podman did.
Selected by `[runtime] kind = "kubernetes"`; the Podman code is untouched behind the
same `boundary::Backend` seam, and both answer the same five checks.

- The node is an unprivileged pod (uid 65532, `drop ALL`, no escalation, seccomp) with
  a ServiceAccount that may create, attach to, read the logs of, and delete pods in its
  own namespace, and read one NetworkPolicy. Those verbs, and no others, are what the
  `runtime` check asserts with a `SelfSubjectAccessReview` per verb.
- **One harness pod per session**, rendered in exactly one place (`runner/kube.rs`) and
  created through the API. Non-root, `drop ALL`, no API token, no service links, no
  resolver (`dnsPolicy: None`), `restartPolicy: Never`. The node holds its stdio over
  `pods/attach`, so the exec pipe is node-owned in the same sense as a child's stdin;
  killing a session deletes the pod.
- **The node is the harness's only route.** A NetworkPolicy selecting
  `tracon.dev/role=harness` admits no ingress and permits egress only to pods labelled
  `tracon.dev/role=node`, on the forward port and the proxy port. The harness resolves
  `tracon-gw` to the node's own pod IP through `hostAliases`, and the node serves the
  CONNECT allowlist proxy itself (`gateway/proxy.rs`: only CONNECT, only 443, the same
  anchored allowlist the gateway file holds). The `network_isolated` check reads the
  policy back and refuses if it is missing, one-sided, or wider.
- **One RWO volume, shared.** The node keeps its state under it; every mount a session
  needs becomes a `subPath` of that claim, so nothing of the node's is reachable except
  what it placed there for that session — a mount source outside the volume cannot be
  rendered at all. Linked-worktree `.git` pointers are absolute, so the claim is mounted
  at the same path in both pods, and harness pods are pinned to the node's Kubernetes
  node because the claim is ReadWriteOnce.
- **The probe is admitted, never scheduled.** The checks create the session pod with a
  scheduling gate and inspect what admission returned, so a mutating webhook that adds
  privilege or a hostPath fails the check rather than passing the rendering.

The manifests a node expects around it are in `deploy/kubernetes/base` and printed by
`tracon setup` on this runtime; the node verifies them and does not create them. The
Coder template that carries this to the work cluster is not in this repository; the
topology was built and proven on a Kubernetes cluster with Cilium network policies.

### Boundary check

At startup, before accepting any session, the node verifies its own boundary: the
container runtime is reachable, the harness image is not privileged, no daemon socket is
mounted into it, and the harness network has no route except the node. If any check
fails, the node refuses to run harnesses and says which check failed. It still serves
the SPA and still relays, so the operator can see the refusal from anywhere.

The checks run against **the same run specification a session uses**, rendered onto a
probe container that is created but never started. A second description of what a session
runs would drift from the first; this way what is verified is what runs. `--deep` adds an
active probe from inside the boundary: no direct egress, an allowlisted host reachable
through the proxy, an unlisted host refused.

An earlier draft had a second mode, `observing`, for nodes that could not establish the
boundary. It was dropped: a node that cannot enforce should not be quietly running
credential-adjacent work with a label on it. Refusal is the honest state, and it is a
startup condition rather than something every screen has to explain.

### Credential classes

Two classes, and conflating them is the most likely way to build a gate that is theater.

| Class | Held by | Notes |
|---|---|---|
| Identity / mesh keys | Node, generated locally, never transmitted | Enrollment via short-lived code from an enrolled node |
| Action credentials | Node broker, sealed, harness has no read path | `glab`, `gh`, `acli`, deploy creds, the consulta DB credential |
| Model auth | Node broker, same store, kinds `api_key` / `oauth` | Injected by the model gateway; the harness holds its session token as a placeholder |

If action credentials sit on the same UID as the harness, the agent has Bash and can read
them. Separate UID, rootless container, or separate container.

This is not hypothetical. consulta today reads its database password from a `.env` on
the same UID as the agent that calls it. Its read-only guard is real, but an agent
with Bash can `cat .env` and open its own connection with no guard at all. That is the
exact shape of a gate that is theater, and it is the first thing the broker fixes.

### Brokered tools

Some things the agent needs are not CLIs to be wrapped but tools the node should
expose directly over MCP, the same way `retain` / `recall` are. The credential never
leaves the node, the harness never sees a process, and channel bindings apply because
the tool is the node's.

**consulta is absorbed as the first of these.** It is a read-only SQL runner (single
`SELECT` / `WITH`, refused before connect, executed in a never-committed read-only
transaction) that is only ever called by agents. The CLI surface (`--sql`, `--file`,
`--param`, `--describe`, `--limit`, `--profile`) is already a tool schema:

| Tool | Input | Notes |
|---|---|---|
| `query` | `sql`, `params`, `limit` | JSON rows out. `--format table/markdown` and `--output` were human affordances and go. |
| `describe` | `table` | Emits the backend's data-dictionary `SELECT`; same guard, same transaction. |

`--profile` becomes a channel binding rather than a flag: which channel may use which
connection, and on which nodes.

Shape: the node ports the guard to Rust (`sqlparser-rs`, a stronger parser than
sqlparse) and refuses before spawning anything. What passes is handed to the Python
sidecar, which the node spawns and owns on its own side of the privilege boundary with
the credential injected from the broker as environment (consulta already prefers
process environment over `.env`). consulta's own guard stays as the second,
independent check. The two-independent-checks property is kept and now straddles a
privilege boundary. The sidecar is the same pattern as a harness adapter: owned
subprocess, pinned version, replaceable.

Why the first: one credential, read-only by construction, smallest blast radius of
anything the broker will hold, and it exercises the whole tool → node → broker →
external call → result path before `gh` and `glab` depend on it.

**GitLab and Jira followed, as verbs rather than CLIs.** `mr_status` and `mr_comment`;
`issue` and `issue_comment`. Opening a merge request stays the review path. Merging,
marking ready, transitioning a ticket, triggering a pipeline, and deploying are not
tools, so the work-channel agreements are the absence of a verb rather than a rule
about one — an agent cannot call what does not exist, and the token that could do it
never leaves the node. Both speak REST from the node; no `glab` or `acli` binary is
needed where the node runs.

**Policy decides every tool call before the broker is touched**, under the kind
`tool`. An allow rule names the tool exactly; a deny returns its reason to the agent;
a tool the bundle does not mention is put to the operator on the session's queue, with
the same expiry and answer path as a harness permission. Adding a tool never widens
what runs unattended.

**A credential is bound to nodes as well as channels.** `nodes = [...]` on a
credential pins it by node id, so a store copied to another machine brokers nothing
there. "consulta on the work node only" is that field.

Note for the port: consulta's MSSQL "read-only" statement is
`SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED`, which is an isolation level and
does not reject writes. Harmless while MSSQL is unverified; the Rust guard must not
treat the backend statement as a guarantee.

### Policy

- Evaluated locally on the node. A hub outage must not stop work.
- Fail closed on anything requiring approval. Fail open on auto-allow.
- Policy bundles signed with a key not present on the hub, so a compromised hub can
  serve stale policy but not new policy.

### Review

The existing `review` contract is absorbed, not replaced: submit an artifact, human
approves/edits/rejects, agent reads the verdict back. What changes is that `glab` and
`acli` move behind the broker, so the approved bytes become the only bytes that can be
posted. Same verdict contract, same `/revise` flow.

**Code edits in a diff go through `/revise`**, not direct application. The agent remains
the only writer to the worktree. Record the blob hash of each file at submit time
regardless, so a stale-diff conflict surfaces rather than silently clobbering later work.

## Workspaces

Sessions run in a git worktree, never a main checkout, because multiple sessions
frequently run against one repo.

```
git worktree add /private/tmp/<repo>-<slug> -b <type>/<KEY>-<desc> origin/<default>
```

- Outside the repo, so no `.gitignore` entry and no harness-specific directory.
- `review` resolves a linked worktree through its `.git` pointer back to the registering
  repo.
- Base is always `origin/<default>` after a fetch.
- A dirty or feature-branched main checkout is left alone and reported.

The node materializes harness config into a scratch directory and passes it explicitly
(`--mcp-config`, settings paths, `--bare` where supported). Instruction-file discovery by
directory walk stops mattering, which removes the current constraint that agents must be
launched from the repo rather than the worktree.

This also retires the generated-and-gitignored `AGENTS.md` / `CLAUDE.md` workaround, and
with it the ugrep footgun where recursive search silently skips those files and an audit
comes back falsely clean.

## Channels and separation

A **channel** is the unit of tenancy: a client, a personal project, or the work
environment. Separation is enforced by key scoping, not row filtering. A node enrolled
for channel A cannot decrypt channel B.

Channels carry bindings:

| Binding | Meaning |
|---|---|
| Processing node | Which node may run embeddings, consolidation, and rollups |
| Provider | Which model provider that node may use for this channel |
| Notification sink | Where approvals surface |
| Brokered tools | Which brokered tools and connections the channel may use, and on which nodes |

Bindings are enforced at the node and fail closed. A node asked to process a channel it
is not bound to refuses rather than falling back to a default provider. Bindings are
recorded as data alongside the event log (channel, node, provider, effective date) so
"where did this content go" is a query rather than a recollection.

This generalizes a pattern an earlier end-to-end-encrypted project already used: trust is extended explicitly to a
named processing node and a named provider.

### Current bindings

| Channel | Processing node | Provider | Sink | Brokered tools |
|---|---|---|---|---|
| Personal | hub | a hosted inference API | phone push | consulta |
| Client | hub | a hosted inference API (permitted by contract) | phone push | consulta |
| Work | work laptop node | Local models only | desktop wrapper | `mr_status` / `mr_comment` (GitLab), `issue` / `issue_comment` (Jira), consulta (work node only, by node binding) |

Work-channel embeddings run locally on the laptop. Embedding models are small enough
that no external provider is required, which removes the contract question entirely.
`gh` is not in that last column on purpose: it is a node-side binary the node runs
itself after an approval, never a tool an agent may call.
Work-channel consolidation runs against a small local model for the same reason.

## The hub

An always-on node in a Kubernetes cluster. It is a **peer with a role**, not a
service tier. The role has two halves that arrive in different phases:

1. **Relay.** Routes ciphertext frames between nodes, keyed per channel. This is all the
   hub does in Phase 2, and it is the only part of the system that must be reachable
   for two nodes to see each other.
2. **Replica and processor.** Sync ordering, embeddings, consolidation, rollups. Phase 4.

Both halves are the same binary in the same pod; the relay half never needs plaintext
and the work channel never gives it any.

### Sync

Hub-and-spoke, not peer-to-peer. Conflicts are only ever pairwise (node against hub),
never n-way, so no CRDT or quorum machinery is required. Nodes stamp writes with site ID
and monotonic sequence; the hub assigns global ordering on receipt; nodes pull from a
cursor. Offline writes queue and push on reconnect. Hybrid logical clocks plus
last-write-wins per record.

**The hub is authoritative for ordering, never for availability.** Every node holds a
full local replica. Reads resolve locally. Sessions start with the hub unreachable.

Distributed SQLite options were evaluated and rejected: rqlite and dqlite need Raft
quorum (a Coder pod that stops nightly breaks it), LiteFS needs a leader with read-only
replicas, libSQL embedded replicas forward writes to a primary so offline writes fail,
and Litestream is single-writer backup. All of them assume a sync layer that can read the
data, which the hub cannot for work channels.

*Built (Phase 4):* the `sync` crate, shared by node and hub. A write stamps
`(site, site_seq)` and an HLC in one transaction with its `change_log` row, then travels
as a `changes` frame on the record's channel; a receiver dedupes on `(site, site_seq)`,
observes the HLC, and applies row-level last-writer-wins on `(hlc_ms, hlc_ctr, site)`
with tombstones for deletes. Catch-up is per site: a sequence gap, a retention `410`, or
a fresh channel key asks each site for its own log after what is held. The replicated
tables are `document`, `memory`, and `promotion`; sessions keep their Phase 2 mirror.

`cr-sqlite` is the one option that fits an opaque relay path: `crsql_as_crr` upgrades a table
and `crsql_changes` produces and applies changesets that can be encrypted and shipped.
Deferred, not rejected. Inserts into CRRs run roughly 2.5x slower, there are caveats
around uniqueness constraints and foreign keys, rich-text CRDTs are unimplemented, and
project activity has been thin. Keep the changeset shape so it remains available.

### Processing

Being the only always-on peer is worth more than the sync:

- Embedding generation on receipt, so a memory written from a sleeping node is
  searchable immediately
- Nightly consolidation and memory promotion
- Metrics rollups
- Somewhere for the phone to talk to

Degraded mode is explicit: **hub unreachable means recall from the local replica only.**
Naming this prevents the hub from quietly becoming load-bearing.

*Revised in Phase 7:* semantic search is not among the things a hub outage costs. The
vector index is node-local — it cannot replicate, because a vector is not a safe form of
encrypted content — so every node embeds its own replica and searches it locally. What a
hub outage costs is the other nodes' writes, not the ability to search meaningfully.
Embedding generation on receipt is therefore each node's own job rather than the hub's.

### Mesh frames

Decided in Phase 2; the wire contract is `spec/README.md` and `proto/` pins it with test
vectors.

A frame is `{ v, id, channel, sender, recipient?, sealing, sent_ms, body, sig }`. The
hub reads `channel` (the routing key) and `sender`, verifies the signature, and stores
the rest as bytes. `sealing` is either **channel** — the body is sealed under the
channel's newest epoch key, readable by every member — or **direct** — sealed to one
node's X25519 key, which is how enrollment handoffs, policy bundles, and commands
travel; the hub relays ciphertext either way. Frames are **sealed then signed**: the
id is a hash of the canonical bytes including the sealed body, the signature covers the
id, and every receiver verifies before opening, so a tampered frame is dropped without
the decrypt path running. The channel and epoch (or sender and recipient) are bound into
the AEAD associated data, so a frame the hub re-labels onto another channel fails to
open there.

Ordering is the hub's: a per-channel sequence assigned on receipt. Nodes pull from a
persisted cursor; the hub's SSE stream is a payload-free hint to pull sooner, never the
source of truth. Replay is closed twice: the hub remembers request signatures within its
freshness window (the signature is the nonce), and nodes remember frame ids. Rekeying
is a new keyring epoch handed to the still-trusted members; old epochs stay in the ring
so retained frames keep opening. The hub keeps fourteen days or 256 MiB per channel; a
cursor behind that gets `410` and resyncs from the owners' periodic snapshots.

One rule the mirror enforces everywhere: **a node speaks only for itself.** Rows naming
another node than the verified sender are dropped and counted.

### Trust asymmetry

The cluster is a managed Kubernetes offering, which is rented infrastructure. The hub is
therefore the highest-value target and the least-controlled box simultaneously.

Resolution: **the hub decrypts personal and client channels. Work channels are handled
opaquely** (ordering and forwarding only, no indexing, no embeddings, ciphertext at
rest). Work-side recall runs FTS-only against the local replica, which is the degraded
mode being built anyway. Per-client keys so a compromise is scoped to one channel.

*Built (Phase 4):* the hub is a member of role `hub` under its own identity; a node
shares a channel with `tracon channel share --hub`, which hands off the keyring with a
`processing: "hub"` binding. The replica (`hub.db`, the same `sync` schema) opens what it
holds keys for and counts what it does not. Opaque is the absence of a key, not a flag.

**Vectors are not a safe form of encrypted content.** Embedding inversion can recover a
substantial amount of source text from a vector alone. Ciphertext at rest plus a
plaintext vector index is a channel with a readable index attached, not a protected
channel.

## Memory

The node owns memory. Harness-native memory is disabled (`memory.backend: off` for omp,
no `CLAUDE.md`). `retain` / `recall` are exposed as MCP tools from the node, with
recalled context injected at session start.

### Why not harness-native

Both anchor to the filesystem. omp's per-project bank derivation is known to silently
fragment memories for the same cwd when git root detection changes, with a parallel
issue for Hindsight and worktrees. The design here is ephemeral worktrees at varying
paths across nodes and containers, which is the pathological case.

**Bank identity comes from the channel and project ID in the mesh. Never from cwd or git
root.**

### Model

Two axes.

**Scope:** global, client, project, session.

**Kind:**

| Kind | Author | Injected | Notes |
|---|---|---|---|
| `directive` | Human only | Always | Build commands, style, conventions. High trust. |
| `fact` | Agent | If high confidence | Durable truth about the code. Carries source, decays. |
| `lesson` | Agent | Only after promotion | Generalized gotchas. Highest value, highest rot. |
| `episode` | System | Never | What happened. Searchable only. |

Only directives and high-confidence facts load automatically. Hard token cap on
injection: oversized context degrades the session it was meant to help.

*Built (Phase 4):* `memory(channel, scope, scope_ref, kind, body, confidence, state)` with
states `active`, `candidate`, `proposed`, `promoted`, `rejected`; `retain` refuses the
directive kind and holds lessons and low-confidence facts as candidates; the orientation
injects directives and facts at or above 0.7 confidence for the session's project.

### Retrieval

FTS5 first, directives ranked above facts. The corpus is small, the highest-value
lookups are exact ("what is the test command"), and vectors are the one part of the
store that does not export as plain text. Pure vector search is bad at exact directive
lookup either way, so FTS is the floor, not a stopgap.

Pin the embedding model. Store model name and dimension on every vector row, or
incremental migration is impossible and stale vectors are undetectable.

**Built (Phase 7).** `sqlite-vec` alongside FTS in the same file, compiled into the
binary rather than loaded as an extension, so the release is still one static file. Two
portability bugs had to be worked around and are recorded in `reference/phase-7-notes.md`;
both would have broken the musl release rather than degraded anything.

The embedder is an OpenAI-shaped `/v1/embeddings` endpoint named in `[embed]` config, not
a model linked into the node. That is what lets the rule above be expressed rather than
merely intended: a work channel points at a `llama-server` on the same machine and
nothing leaves it, while another channel may name a `[providers]` entry and go through
the gateway, where its provider binding and daily ceiling still apply.

A document is chunked on its headings — one vector for a whole guide averages away the
paragraph that answers the question — and each chunk records its span, so a hit shows the
text it matched. A memory is atomic already and embeds whole. The fused ranking keeps the
tiers: the vector contribution is bounded below one tier step, because "directives above
facts" is a decision about whose instructions win, not a relevance heuristic for a better
signal to overrule.

No reranker. FTS plus vectors was enough for a corpus this size; a reranker waits for the
same kind of evidence that vectors did.

Phase 0 found that the hosted inference API in question exposes embeddings synchronously
but does not include them in batch inference, and its reranker is a knowledge-base
feature rather than a standalone endpoint. BGE-M3 and Qwen3 Embedding 0.6B remain the candidates for the
local endpoint; model availability and pricing are operational facts, not permanent
architecture.

### Curation

Auto-retain produces volume. Memory promotion is routed through the existing approval
queue, batched nightly. This is the difference between a memory system and a landfill,
and it is the cheapest possible reuse of the gate.

## Documents

Same store, separate table. Memory and documents have different lifecycles and
conflating them breaks both.

| | Memory | Document |
|---|---|---|
| Size | Small, atomic | Long |
| Author | Mostly agent | Human or co-authored |
| Retrieval | Similarity | By name, with similarity as an entry point |
| Use | Injected | Read deliberately |
| Lifecycle | Decays | Edited as a whole artifact |

The `<type>-<slug>.md` filename prefix scheme (`note-`, `repo-`, `meeting-`, `inbox-`,
`proposal-`, `plan-`, `guide-`, `ref-`, `architecture-`) is already a kind column. Keep
it.

**Memories point at documents.** A memory entry saying "deploy process is in
`ref-deploy-process`" costs twenty tokens; the agent fetches the full document only if
the task needs it. One retrieval index across both with a `kind` discriminator,
documents chunked but always returned with their slug.

*Built (Phase 4):* `document(channel, slug, kind, title, body, hash)` with an FTS5 index;
`doc_search` returns slugs and snippets, `doc_read` the body, `doc_write` is asked;
edits carry the hash last read (`If-Match`, 412 with the current state). The Documents
screen reads, searches, and edits; `tracon doc import|export` move a directory of
markdown in and out as plain files.

### Generated orientation files

The two workspace `README.md` files (personal and work) drift because there is no
mechanism holding them together. In this system that file is generated per session:
shared conventions from the doc corpus, node facts filled from what the node knows about
itself, channel policy layered on top, materialized into the scratch config directory.

The shared portion is small. Conventional commits, branch naming, the code comments
philosophy, the filename prefix scheme. Everything else is environment-specific, and the
work-side ownership table (maintains / shares / someone else's) has no personal
equivalent. This is three layers assembled per session, not one file with conditionals.

`~/.workspace-notes.git` and `workspace-notes-sync` are what the hub replaces. Run both
until the corpus has landed and the two machines demonstrably converge, then cut.

*Built (Phase 4):* `corpus::orientation::assemble` — guides on the channel, this node's
facts, the bundle's deny rules and their reasons, then what is known — delivered as a
read-only file passed to the harness with `--append-system-prompt`, and recorded as an
`orientation` event. The workspace README is imported as `guide-workspace`, and the
shared conventions are the part of it worth keeping short.

## Work ledger

Beads-inspired, not Beads. Beads now uses Dolt for version-controlled SQL with cell-level
merge and sync via Dolt remotes, which means a second distributed sync system with
different consistency semantics running alongside the relay. It also wants an
`AGENTS.md` in the repo.

Three things worth stealing:

1. **Ready-work query.** Topological sort done deterministically, serving only unblocked
   items rather than dumping the graph for the model to parse. The tool thinks, the model
   picks.
2. **Hash IDs.** Nodes must mint work items offline during a hub outage without
   collision.
3. **`discovered-from` edge.** Work found mid-session is recorded and linked to its
   origin instead of evaporating when the session dies. This is what replaces scanning
   markdown for `- [ ]` lines.

Storage is the node's SQLite, namespaced to the channel. The repo constraint is satisfied
by construction.

**The advantage over Beads is structural enforcement.** The known failure is context rot:
the agent that checked ready-work at session start has forgotten the ledger by hour two,
and the upstream mitigation is to kill sessions after each item. Beads can only ask
nicely in `AGENTS.md`. The node owns the harness lifecycle, so it can require a work item
to open a session, inject ready-work at start, and end the session at item close.

*Built (Phase 5):* `work_item` is a replicated table in the `sync` crate (step 2 of its
schema), so the ledger converges the way documents do and the hub holds it for the
channels it reads. Ids are `sha256(channel ‖ project ‖ site ‖ created_ms ‖ title)`;
readiness is never stored — `tracon_sync::work::status` derives it from `deps_json` and
`state` with a Kahn pass over the open subgraph and ids as the final tiebreak, so every
replica lists the same order. Unknown deps block (and say "not seen here"); cycles block
every member. `Manager::create` refuses a plan or execute session whose item is missing,
closed, blocked, or held by a live session; the orientation carries the item and the
project's ready work; `work_discover` records what a session finds, linked to its item;
`work_close`, a close from the interface, or a publish ends the holding session at the
end of its turn (`EndReason::ItemClose`). Events inherit the session's item, so per-item
metrics are a group-by.

## Sessions and phases

Plan, execute, and review are **separate sessions the node spawns**, not phases inside one
session.

This sidesteps subagent model inheritance entirely. Claude Code defaults subagents to the
parent model, which drives cost rapidly on an expensive model; making each phase its own
session means each gets an explicit model on the command line and there is nothing to
inherit.

Two rules:

- A session spec with no model named is a validation failure at spawn, not a surprise
  later.
- Every session carries a budget the node enforces by killing it. Claude Code's result
  message carries `total_cost_usd` and token counts, so the meter is free.

Harness-level routing (omp roles) is a second layer, not the control.

**The budget is denominated in tokens, and dollars are derived.** Most sessions run on a
subscription, where there is no per-session dollar cost to enforce against; tokens are
what every harness reports and what the node can act on alone. A channel's provider
binding may carry a price, and only then does a cost appear. "Cost per accepted change"
in [Metrics](#metrics) is therefore tokens per accepted change, priced where priceable.

**Enforcement is at turn end.** ACP reports usage when a turn completes, not
continuously, so one long turn can overshoot its budget before the node can act. This is
a property of what the harness reports rather than a gap to paper over: the interface
says the budget is checked at turn end, and mid-turn enforcement waits on a usage
snapshot the adapter does not have. Per-channel daily ceilings are Phase 5.

*Built (Phase 5):* a session spec names its `phase` (`plan`, `execute`, `review`). The
operator starts plan and execute sessions; the plan session's purpose is one document,
`plan-<item>`, which its `doc_write` may write unasked and which ends the session
(`phase_done`); execute is refused until that document exists (a channel may bind
`phases.execute.requires_plan=false`). Review sessions are spawned by the node at submit
(below) with the model the channel binds under `phases.review.model` — the spec is
constructed on the node, so there is nothing a harness could inherit. Budgets default from
`phases.<phase>.budget_tokens` on the channel. The daily ceiling is
`ceiling_tokens_per_day` in the channel's bindings, counted from the gateway's
`model_usage` since local midnight: `POST /api/sessions` refuses at 429 with the figures,
and the gateway refuses every model call at 429 so a running session stops spending —
the harness sees the error, a `ceiling` event is recorded once, and the operator decides.

### Supervision

Anything checkable deterministically is checked deterministically. The node runs
`just check`, `just analyse`, `just test`, Pint, and Larastan between phases and feeds
failures back. Model supervision is reserved for judgment with no test. A cheap model
watching an expensive model work mostly pays twice to learn what the test suite would
have reported.

*Built (Phase 5):* at `submit_review`, after the diff is captured and before a human or a
model reads it, the node runs the worktree's `.tracon/checks` (one command per line; else
`[supervision] checks`, default `just check`) in a throwaway container from the harness
image with the worktree mounted at `/work` and nothing else — no credentials, no gateway
token, no MCP — under `[supervision] timeout_secs`. The session shows `waiting_on_check`;
each result is a `check_result` event with the exit code and a 4 KiB tail; the first
failure refuses the submission with that tail and no review exists; the passing list is
recorded on the review (`checks_json`) and shown on its card.

### Review sessions

**Review the diff with a session that never saw the implementation.** A model that
watched itself reason toward a design will rationalize it. A fresh session given only
requirements and diff will not. This is the right use of the expensive model, and it is
cheap because the diff is small relative to the session that produced it.

Cap diff size at submit. Complexity accretes because nothing says no at submission time.

Gate the execute phase on a plan artifact, which converts "requirements first, plan
second" from a request into a mechanism.

*Built (Phase 5):* when the channel binds `phases.review.model`, a review-phase session is
spawned for every submission (and resubmission): same item, a fresh worktree at the
reviewed commit (`worktree::create_at`), an orientation of the item, its plan, and the
diff, and only `recall`, `doc_read`, `doc_search`, and `review_verdict` offered — a call
to anything else is refused by name. Its verdict (approve or request changes, a summary,
findings with severity and path) lands on the review row and the approval screen above
the human's controls, which are unchanged. The cap: `[review] max_diff_lines` (800,
added + removed) and `max_files` (40), refused before the checks with "split the
change" and a `review_rejected` event.

## Metrics

Event log, append-only, replicated.

- **Wall clock and agent time** fall out for free.
- **Human time does not.** It needs an explicit signal, and without one you are
  measuring queue latency and calling it attention. **Decided: claim/release on
  approval items.** A browser heartbeat measures tab focus, not attention, and puts the
  SPA in the loop for a metric. Decided before the schema because it is expensive to
  retrofit.
- **Clock skew across nodes will wreck this.** Record durations monotonically on the node
  that observed them and ship durations, never timestamps to be subtracted across
  machines.

The numbers that matter at six months: approvals per accepted change, cost per accepted
change.

**Provenance.** When agent-written code ships under your name, the trail should answer
which model, which prompt, which approval, which policy version, queryable per commit.
The data is captured for metrics anyway.

**Cost ceilings** per day per channel, enforced by the node, not a dashboard checked
afterward. Runaway spend is the failure mode that scales with how well the system works.

*Built (Phase 5):* `GET /api/metrics` (`tracon metrics`, the Metrics screen) computes per
channel, from the node's own tables: accepted and rejected changes, approvals (permission
answers plus review verdicts) per accepted change, gateway tokens per accepted change
(the implementing and review sessions behind each accepted review), total tokens, cost
only where `[providers.<p>.price]` gives dollars per million tokens, human seconds
(permission request→answer, monotonic on the observing node; review claim→decision), and
agent seconds. Stated as "as seen from this node": usage is counted where the model call
was made, sessions and events mirror. Hub-side rollups are not built. Provenance:
`GET /api/provenance/{sha}` (`tracon provenance`) joins the review found by the reviewed
sha or the published URL to its item and plan, the implementing and review sessions
(model, phase, policy version, budget), the prompts, the approvals, the checks, and the
review session's verdict.

## Clients

Every node serves the same embedded SPA. Client type is a matter of shell.

### PWA

The one capability with no other path: laptops sleep, Coder workspaces stop, the
cluster pod does not.

- **The phone is not a node.** No local replica, no keys at rest, useless offline.
  iOS evicts PWA storage. This is a deliberate exception to the every-node-replicates
  rule. *Revised in Phase 6:* it reads a node directly over an HTTPS ingress with a
  session cookie rather than over the hub — the hub still never talks to a browser.
  See "Reaching a node".
- A backgrounded PWA cannot hold a socket. Notifications arrive as Web Push,
  sent by the node the phone subscribed at.
- **Scope is directing work, not writing code.** Read a diff, approve or reject, send a
  prompt, read output, kill a stuck session.

**Built (Phase 6).** A manifest, icons, and a hand-written service worker. The worker
is deliberately not an offline mode: it caches the shell and the content-hashed assets
and never touches `/api`, because a cached queue you cannot act on is worse than one
that says the node is unreachable — and wrapping an endless SSE response in a fetch
handler breaks the stream outright. Nothing is stored beyond that; the only credential
is an HttpOnly cookie the browser holds.

### Desktop wrapper

Tauri, not Swift plus Qt. What is actually wanted from native is tray presence,
command-tab, global hotkey, and system notifications. Tauri provides all four, and it is
Rust, so on desktop the node and its shell are one binary sharing types with no invented
serialization boundary.

**The wrapper supersedes the earlier menu-bar supervisor**, which retires. The node
itself is supervised by the platform (launchd on macOS, systemd user units on Linux,
container lifecycle in Coder) or, since Phase 7, by the wrapper itself; the wrapper's
role as a service toggler is not reimplemented in Tauri, which would be a lot of
platform surface for what is wanted from native.

- Collapses to tray when not in use, so it command-tabs alongside Teams and Outlook.
- **No terminal.** Deferred until genuinely needed.
- **Editing is diff editing, not an editor.** `@codemirror/merge` for editable unified
  diffs, reusing the CodeMirror the earlier tools already used. Edits become
  `/revise` submissions.

**Built (Phase 6).** The wrapper is a tray client and nothing more: it supervises
nothing, holds no session state, and loads the same interface the node serves. Its own
cargo workspace, so the node and hub stay buildable where webkit and gtk headers are
absent. The tray is the queue plus a kill switch — the kill one level down, since it is
destructive and easy to mis-click — and anything worth reading opens the window at that
route.

The diff editor edits the file the review submitted, shown as a unified merge view
against the text it changed from, and emits a patch. The patch travels with the notes:
`review_status` hands it to the agent, which applies it and resubmits. So editing is
still a *request*, and the agent is still the only writer to the worktree. Desktop only,
and the editor is lazy-loaded so the phone never fetches it.

**Built (Phase 7): it can run the node too.** The node does not daemonize, so
something has to run it, and on a laptop that something is better as a tray icon
than a unit file — the node is wanted while you are logged in. It adopts a node
that is already answering rather than starting a second one, because two over
one state directory would fight over the same SQLite file and harness socket, so
`tracon service install` stays correct for a machine that must survive logout.
Quitting stops it with the same SIGTERM-and-wait the unit uses, since shutdown
tears down session containers and cutting that short leaks them.

### Reach across nodes

The interface talks only to the node that served it. That node mirrors every peer's
sessions, permission requests, and reviews into its own tables, scoped by node, and
forwards commands for sessions it does not own to the owner as sealed frames. A verdict
is executed on the owner, because staleness and publishing need the owner's worktree
and broker. The hub never talks to a browser; the phone's read-over-the-hub path is a
later phase.

### Client crash invariant

Sessions live in the node. A client crash is a reconnect, never lost work. This is the
specific failure of VS Code Remote that motivated the design, so it is stated as an
invariant: **no session state in the shell.**

Two things a person types are not yet session state: an unsent prompt and an
in-progress diff edit. **The node holds unsent prompt drafts per session**, so a phone
eviction or a crashed tab loses nothing typed. Diff edits are desktop-only and stay in
the browser's local storage until submitted as `/revise`; that is the one piece of
client-held state, and it is confined to the surface least likely to lose it.

## Notification sinks

| Surface | How |
|---|---|
| Phone (any channel) | Web Push from the node the phone subscribed at, sealed to the phone's key |
| Desktop | The wrapper's tray, from the queue it already reads |

**Built (Phase 6, reworked in Phase 8).** Every node pushes to the devices subscribed
at *it*; there is no bound delivering node. A phone subscribes wherever it logs in,
and a queue mirrored onto three nodes reaches a phone subscribed at one of them once.
A phone logged into two nodes hears from both, and the banner's `tag` — the same on
every node for the same item — makes the second replace the first rather than stack.
Whether a channel notifies at all is the `notify.enabled` binding, on by default: the
subscribed device is the opt-in now.

The delivering task reads the *bus*, not the session manager: a peer's approval
arrives mirrored and is published untapped, which reaches subscribers but never
`Manager::publish_queue`. Hooking the manager would have silently missed the case the
phone exists for — the other laptop raised it and this node is the one awake.

Delivery is RFC 8291 payload encryption and RFC 8292 VAPID, done in the node over
pure-Rust crates so the static build stays static. The node holds one VAPID key per
node (in `kv`, never replicated) and, per device, only the subscription's public half;
a push service sees ciphertext and a signature naming the node, and the phone's service
worker sees a title, a body and a path on the node's own origin. A subscription is tied
to the browser session that registered it, so revoking a login silences its devices.
Pushes are hints: the queue is the truth, so a failed send is retried once and dropped,
a `410` forgets the device, and a slow push service can never block a session.

What it does not do is as deliberate: it does not announce the standing queue at
startup (a redeploy is not news), and it does not page again when a review returns to
`new` because the operator opened it and walked away — only when the agent resubmits.

## Reaching a node

Three surfaces, three different answers, and the differences are the point.

**The harness** reaches its own tools over a separate listener with a per-session
bearer token. It never carries the operator API, so a harness that finds the forward
cannot drive sessions.

**The operator, on the node's own machine**, is the operator by definition: a shell
there already has the state. Loopback callers are answered with no credential, guarded
only against DNS rebinding — a page on `evil.example` that resolves to `127.0.0.1`
still sends `Host: evil.example`, which is refused.

**The operator, from anywhere else**, needs a token. `tracon auth issue` mints one and
prints it once; `POST /api/login` exchanges it for an HttpOnly, Secure, SameSite=Lax
cookie. A cookie rather than a header because `EventSource` cannot set headers and the
phone lives on `/api/stream`; Lax rather than Strict because opening a review from a
push notification is a top-level cross-site navigation, and Strict drops the cookie on
exactly that. Non-browser clients present the token as a bearer instead.

Only hashes are stored — of the token, and of each cookie — so reading the database
mints neither. Rotating the token drops every logged-in client in the same transaction
that writes the new hash, which makes `tracon auth issue` the revoke-everything button.
A credential alone is not enough: Origin and Host must agree, so a cross-site page
carrying the cookie is refused.

Until a token exists the node answers loopback only and says so, naming the command
that would open the door. The SPA shell stays public once one does — it is open-source
code holding nothing, and serving it is what lets the login screen render and a
notification's deep link open.

**The phone**, therefore, reaches a node directly over an HTTPS ingress rather than
through the hub. The earlier sketch had it "read over the hub", but the hub has no
browser-facing surface and no auth model for one, and inventing both to reach a node
that is already reachable would have been the larger change. The hub still never talks
to a browser.

## Constraints

### Single user

There is no tenancy in the auth model. One human holds keys. Channels are cryptographic
separation for one operator's contexts, not multi-user isolation. This is deliberate.
Recorded here so that nobody, including the author in a year, attempts to retrofit
multi-user onto it.

### Personal accounting is out of scope

Accounting stays in the business tool domain. tracon owns node, session, event,
work item, memory, document, and policy. It does not grow clients, invoicing, or a
business domain. Project separation is channels, not a `Client` table.

### Bootstrap

Once bootstrapped, the node is developed using agents that run inside the node. A
documented path to running a harness directly, outside the system, must be maintained,
or a bad deploy locks out the tool needed to fix it.

Phase 1 may be implemented by an external harness on any environment that can modify
and test the repository; that environment need not be the Phase 1 runtime host. tracon
builds tracon after Phase 1 exits. Rebuilding and restarting the node remains a host-side
recipe, never something a session does, so a session that breaks the build cannot lock
the operator out of the running node.

## Data lifecycle

**Backups.** Once the hub is the source of truth for memory, docs, work items, and
metrics, it is one persistent volume. Encrypted snapshots to object storage, with a restore
path that has actually been exercised. Hub failure is survivable; hub failure without a
tested restore costs years of accumulated context.

**Deletion.** Retention per kind, and a real delete that propagates. Client channels
especially: engagements end and removal may be contractual. Deleting from a replicated
store is harder than adding to one, so tombstone semantics are decided before there is
data.

**Export.** Plain-text export for every kind. No format readable only by this binary.

## Open questions

1. **Retention and tombstone semantics.** Needs deciding before data accumulates.
2. **Stacked MR handling.** Whether the node owns restacking (the `blocks` edge in the
   work ledger is already the branch base relationship) or whether feature flags on trunk
   make stacks unnecessary. The trunk plus semantic-release setup favors flags.

Resolved since the first draft: the hub is the relay (see [Topology](#topology)); the
wrapper delegates supervision to the platform (see [Clients](#clients)); human time is
claim/release (see [Metrics](#metrics)); the mesh frame format (see
[Mesh frames](#mesh-frames), Phase 2).

## Prior art and salvage

tracon absorbed a handful of the operator's earlier tools rather than living beside
them: a review contract (advisory there, enforced by the broker here), a document
notes app (its corpus and `<type>-<slug>` prefix scheme kept), a database helper
(`consulta`, now the node's MCP tools with the guard contract kept, the repo retiring
once the tools have been in real use), an end-to-end-encrypted relay (its trust-binding
pattern reused in the hub), a menu-bar service supervisor (superseded by the wrapper;
its display-linked theme switcher was **deliberately killed** in Phase 6 rather than
rehomed), and a phone-push bridge (retired in Phase 8, when push moved into the node).
Personal accounting stayed where it was.

## What rots and what does not

Yegge's arc is instructive: Gas Town, then Gas City as an SDK, then Wheelhouse, a
closed-source harness built for one person, having given up on reusable harnesses.

The parts of this system that will rot are the harness adapters and anything tracking
harness features. The parts that keep paying are the credential broker, the brokered
tools, the review gate, the memory and document corpus, and the work ledger. Build so that when the orchestration
layer becomes obsolete, the corpus and the ledger outlive it.
