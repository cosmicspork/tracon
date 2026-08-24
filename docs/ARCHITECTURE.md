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
                        │   hub (homelab)      │
                        │   always-on          │
                        │   relay: routes      │
                        │   ciphertext per     │
                        │   channel            │
                        └──────────┬───────────┘
                                   │
                 ┌─────────────────┼─────────────────┐
                 │                 │                 │
           ┌─────▼──────┐   ┌──────▼─────┐   ┌───────▼────┐
           │ work node  │   │ personal   │   │  PWA /     │
           │ (Coder pod)│   │ node       │   │  desktop   │
           │            │   │ (Bazzite)  │   │  clients   │
           └─────┬──────┘   └────────────┘   └────────────┘
                 │ docker exec
           ┌─────▼──────────┐
           │  devcontainer  │
           │  (harness)     │
           └────────────────┘
```

Every node dials out to the hub. Nothing accepts inbound connections. A Coder
workspace with no ingress and a laptop are identical to the mesh.

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
[Kritee](#kritee-is-out-of-scope)).

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

Fetched as a binary by a one-line bootstrap. **Not** shipped via the dotfiles repo. The
dotfiles repo is configured for interactive human use and is heavy; agent-only
environments should not pay for it, and coupling the two means every node change
requires a dotfiles change.

## Harness control

### Protocol

**ACP (Agent Client Protocol) is the adapter interface.** It carries session management,
tool execution, and interactive permissions over JSON-RPC on stdio, and provides
filesystem routing, terminal routing, and permission prompts without bespoke glue.

| Harness | Mode | Notes |
|---|---|---|
| omp | `omp acp` | Also exposes `_omp/*` for session discovery and reopen. Non-spec. |
| opencode | `opencode acp` | Brings up an HTTP server; see cautions below. |
| Claude Code | native | `--input-format stream-json --output-format stream-json --permission-prompt-tool stdio`, one resident process, messages on stdin. |

Claude Code is the special case. Its `control_request` / `control_response` path is
functionally equivalent to ACP permission requests but is not ACP.

The adapter trait exists from the first commit; only the omp adapter is built until a
concrete task needs another. Adapters are the part of the system that rots, and three
of them on day one is scope.

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

Model credentials stay in the harness's own store (`~/.omp/agent/agent.db`,
`~/.local/share/opencode/auth.json`). The node does not broker them. Session refresh is
the harness's problem.

Because the harness runs in an ephemeral devcontainer, that state directory must be a
volume the node owns and mounts. `omp-docker` establishes this pattern with
`OMP_STATE_DIR`; copy it rather than inventing one.

Work-side model access is through the employer's provider subscription via
omp/opencode. Subscription access and API-platform access are separate systems at
every major provider, so there is no assumption of a programmatic API endpoint on the
work side.

## The gate

The review server becomes a real gate rather than an advisory one.

### Privilege boundary

The gate only works if the harness cannot reach what the node holds. **Every node
establishes this boundary or does not run harnesses.** There is no advisory mode.

In the work topology the boundary is the existing docker-in-docker layout:

- Node runs in the outer Coder pod
- Harness runs in the inner devcontainer
- Node spawns the harness via `docker exec -i` and holds the pipe
- Node keeps the Docker client; the inner container never gets one

On the personal machine (Bazzite) the same shape is built with rootless podman, and it
was proven by hand in Phase 0 (see `VALIDATION.md`). The node owns three things:

- an **internal network** (`--internal --disable-dns`), which has no route to the
  internet and, in rootless podman, none to the host either;
- a **gateway container** on both that network and the default one, running an HTTP
  CONNECT proxy with a default-deny allowlist of model provider hosts, and forwarding
  one internal port to the node's unix socket;
- the **harness container** on the internal network only, `--cap-drop=ALL`,
  `no-new-privileges`, `HTTPS_PROXY` pointed at the gateway, node holding the exec pipe.

The harness therefore reaches exactly two things: allowlisted provider hosts through
the proxy, and the node through the gateway. A unix socket mounted straight into the
harness does not work under SELinux without `label=disable`, which is why the gateway
carries it instead. The hub runs no harnesses and needs no boundary.

Three things collapse this boundary and must be verified per environment:

1. **Docker socket in the devcontainer.** The docker-in-docker or
   docker-outside-of-docker devcontainer features, or a mounted
   `/var/run/docker.sock`, give the agent the daemon API and therefore everything.
   Devcontainer templates enable this by reflex. Check first.
2. **Privileged inner container.** The outer pod needs privilege for DinD; the inner one
   must not have it.
3. **Egress.** The devcontainer goes on an internal Docker network with the node as the
   only reachable address. Without this, the agent does not need brokered credentials for
   anything it can do with a token found in repo history. Image build needs network;
   open at build, closed at run.

### Boundary check

At startup, before accepting any session, the node verifies its own boundary: the
container runtime is reachable, the harness image is not privileged, no daemon socket is
mounted into it, and the harness network has no route except the node. If any check
fails, the node refuses to run harnesses and says which check failed. It still serves
the SPA and still relays, so the operator can see the refusal from anywhere.

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
| Model auth | Harness store | See above |

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

This generalizes the pattern svastha already uses: trust is extended explicitly to a
named processing node and a named provider.

### Current bindings

| Channel | Processing node | Provider | Sink | Brokered tools |
|---|---|---|---|---|
| Personal | hub | DigitalOcean GradientAI | pager | `gh` |
| Client | hub | DigitalOcean GradientAI (permitted by contract) | pager | `gh` |
| Work | work laptop node | Local models only | desktop wrapper | `glab`, `acli`, consulta (work Coder node only) |

Work-channel embeddings run locally on the laptop. Embedding models are small enough
that no external provider is required, which removes the contract question entirely.
Work-channel consolidation runs against a small local model for the same reason.

## The hub

An always-on node in the homelab Kubernetes cluster. It is a **peer with a role**, not a
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

Degraded mode is explicit: **hub unreachable means FTS-only recall from the local
replica, no semantic search.** Naming this prevents the hub from quietly becoming
load-bearing.

### Trust asymmetry

The homelab is DigitalOcean Kubernetes, which is rented infrastructure. The hub is
therefore the highest-value target and the least-controlled box simultaneously.

Resolution: **the hub decrypts personal and client channels. Work channels are handled
opaquely** (ordering and forwarding only, no indexing, no embeddings, ciphertext at
rest). Work-side recall runs FTS-only against the local replica, which is the degraded
mode being built anyway. Per-client keys so a compromise is scoped to one channel.

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

### Retrieval

FTS5 first, directives ranked above facts. The corpus is small, the highest-value
lookups are exact ("what is the test command"), and vectors are the one part of the
store that does not export as plain text. Add `sqlite-vec` alongside FTS in the same
file (no server) and a reranker on top only once FTS demonstrably misses things in
real use. Pure vector search is bad at exact directive lookup either way, so FTS is
the floor, not a stopgap.

Pin the embedding model. Store model name and dimension on every vector row, or
incremental migration is impossible and stale vectors are undetectable.

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

The notebook `<type>-<slug>.md` prefix scheme (`note-`, `repo-`, `meeting-`, `inbox-`,
`proposal-`, `plan-`, `guide-`, `ref-`, `architecture-`) is already a kind column. Keep
it.

**Memories point at documents.** A memory entry saying "deploy process is in
`ref-deploy-process`" costs twenty tokens; the agent fetches the full document only if
the task needs it. One retrieval index across both with a `kind` discriminator,
documents chunked but always returned with their slug.

### Generated orientation files

The two workspace `README.md` files (personal and work) drift because there is no
mechanism holding them together. In this system that file is generated per session:
shared conventions from the doc corpus, node facts filled from what the node knows about
itself, channel policy layered on top, materialized into the scratch config directory.

The shared portion is small. Conventional commits, branch naming, the code comments
philosophy, the notebook prefix scheme. Everything else is environment-specific, and the
work-side ownership table (maintains / shares / someone else's) has no personal
equivalent. This is three layers assembled per session, not one file with conditionals.

`~/.workspace-notes.git` and `workspace-notes-sync` are what the hub replaces. Run both
until the corpus has landed and the two machines demonstrably converge, then cut.

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
   origin instead of evaporating when the session dies. This is what replaces notebook's
   `- [ ]` dashboard scanning.

Storage is the node's SQLite, namespaced to the channel. The repo constraint is satisfied
by construction.

**The advantage over Beads is structural enforcement.** The known failure is context rot:
the agent that checked ready-work at session start has forgotten the ledger by hour two,
and the upstream mitigation is to kill sessions after each item. Beads can only ask
nicely in `AGENTS.md`. The node owns the harness lifecycle, so it can require a work item
to open a session, inject ready-work at start, and end the session at item close.

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

### Supervision

Anything checkable deterministically is checked deterministically. The node runs
`just check`, `just analyse`, `just test`, Pint, and Larastan between phases and feeds
failures back. Model supervision is reserved for judgment with no test. A cheap model
watching an expensive model work mostly pays twice to learn what the test suite would
have reported.

### Review sessions

**Review the diff with a session that never saw the implementation.** A model that
watched itself reason toward a design will rationalize it. A fresh session given only
requirements and diff will not. This is the right use of the expensive model, and it is
cheap because the diff is small relative to the session that produced it.

Cap diff size at submit. Complexity accretes because nothing says no at submission time.

Gate the execute phase on a plan artifact, which converts "requirements first, plan
second" from a request into a mechanism.

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

## Clients

Every node serves the same embedded SPA. Client type is a matter of shell.

### PWA

The one capability with no other path: laptops sleep, Coder workspaces stop, the homelab
pod does not.

- **The phone is not a node.** No local replica, no keys at rest, reads over the hub,
  useless offline. iOS evicts PWA storage. This is a deliberate exception to the
  every-node-replicates rule.
- A backgrounded PWA cannot hold a socket. Notifications go through pager.
- **Scope is directing work, not writing code.** Read a diff, approve or reject, send a
  prompt, read output, kill a stuck session.

### Desktop wrapper

Tauri, not Swift plus Qt. What is actually wanted from native is tray presence,
command-tab, global hotkey, and system notifications. Tauri provides all four, and it is
Rust, so on desktop the node and its shell are one binary sharing types with no invented
serialization boundary.

**The wrapper supersedes Switchboard**, which retires. The node itself is supervised by
the platform (launchd on macOS, systemd user units on Bazzite, container lifecycle in
Coder); the wrapper is a tray client of a node that is already running, not its
supervisor. Switchboard's units move; its role is not reimplemented in Tauri, which
would be a lot of platform surface for what is wanted from native.

- Collapses to tray when not in use, so it command-tabs alongside Teams and Outlook.
- **No terminal.** Deferred until genuinely needed.
- **Editing is diff editing, not an editor.** `@codemirror/merge` for editable unified
  diffs, reusing the CodeMirror already present in review and notebook. Edits become
  `/revise` submissions.

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

Bound per channel like processing nodes.

| Channel | Sink |
|---|---|
| Personal, client | pager (E2E-encrypted phone push, already built) |
| Work | Desktop wrapper tray |

Work approvals do not require the phone, which removes the need to tunnel anything out
of the work machine and eliminates the asymmetry that would otherwise need explaining.

## Constraints

### Single user

There is no tenancy in the auth model. One human holds keys. Channels are cryptographic
separation for one operator's contexts, not multi-user isolation. This is deliberate.
Recorded here so that nobody, including the author in a year, attempts to retrofit
multi-user onto it.

### Kritee is out of scope

Kritee stays in the business tool domain. tracon owns node, session, event,
work item, memory, document, and policy. It does not grow clients, invoicing, or a
business domain. Project separation is channels, not a `Client` table.

### Nothing in project repos

No `.claude/`, no `AGENTS.md`, no `.beads/`, no tracon files of any kind.

### Bootstrap

The node is developed using agents that run inside the node. A documented path to
running a harness directly, outside the system, must be maintained, or a bad deploy locks
out the tool needed to fix it.

Phase 1 is built directly on the host; tracon builds tracon from Phase 2 on. Rebuilding
and restarting the node is a host-side recipe, never something a session does, so a
session that breaks the build cannot lock the operator out of the running node.

## Data lifecycle

**Backups.** Once the hub is the source of truth for memory, docs, work items, and
metrics, it is a DigitalOcean PVC. Encrypted snapshots to object storage, with a restore
path that has actually been exercised. Hub failure is survivable; hub failure without a
tested restore costs years of accumulated context.

**Deletion.** Retention per kind, and a real delete that propagates. Client channels
especially: engagements end and removal may be contractual. Deleting from a replicated
store is harder than adding to one, so tombstone semantics are decided before there is
data.

**Export.** Plain-text export for every kind. No format readable only by this binary.

## Open questions

1. **Mesh frame format.** Envelope shape, replay protection, ordering guarantees,
   rekeying. Unchanged by merging the relay into the hub.
2. **Retention and tombstone semantics.** Needs deciding before data accumulates.
3. **Stacked MR handling.** Whether the node owns restacking (the `blocks` edge in the
   work ledger is already the branch base relationship) or whether feature flags on trunk
   make stacks unnecessary. The trunk plus semantic-release setup favors flags.

Resolved since the first draft: the hub is the relay (see [Topology](#topology)); the
wrapper delegates supervision to the platform (see [Clients](#clients)); human time is
claim/release (see [Metrics](#metrics)).

## Prior art and salvage

| Repo | Disposition |
|---|---|
| `review` | Contract absorbed. Broker makes it enforcing. Repo retired. |
| `notebook` | Document corpus absorbed. Prefix scheme kept. Repo retired. |
| `switchboard` | Superseded by the desktop wrapper. Retired last, after it stops supervising the things being replaced. The display-linked theme switcher is unrelated and needs its own home or a deliberate kill. |
| `pager` | Kept. Becomes the notification sink for personal and client channels. |
| `svastha` | Relay and trust-binding patterns reused. |
| `consulta` | Absorbed as node MCP tools (`query`, `describe`) with a node-owned Python sidecar. Guard contract kept. Repo retired after the tool has been in real use. |
| `kritee` | Out of scope. Unchanged. |

## What rots and what does not

Yegge's arc is instructive: Gas Town, then Gas City as an SDK, then Wheelhouse, a
closed-source harness built for one person, having given up on reusable harnesses.

The parts of this system that will rot are the harness adapters and anything tracking
harness features. The parts that keep paying are the credential broker, the brokered
tools, the review gate, the memory and document corpus, and the work ledger. Build so that when the orchestration
layer becomes obsolete, the corpus and the ledger outlive it.
