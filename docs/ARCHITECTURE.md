# Architecture

The rules. This document holds the commitments, invariants, and boundaries that
constrain everything else, with rationale where the reasoning is not obvious. It
deliberately does not describe features: what each phase built, and what building it
changed, is recorded in [reference/](reference/) so this file does not drift as the
features do. Decisions here are settled unless listed under
[Open questions](#open-questions).

`tracon`: named for terminal radar approach control. The facility sequences aircraft
and issues clearances; it never flies one. The node supervises and gates; it never
reasons.

## Problem

Agent work driven from TUIs on laptops produces four distinct problems: latency (a
TUI round-trips every keystroke; a chat interface round-trips once per prompt),
environment sprawl (agents running locally against remote environments force
devcontainers on the laptop just to run tests), no enforcement (working agreements
are prose the agent may not still have in context an hour in), and no visibility when
the laptops are closed. The system is a supervisor and control plane between the
human and existing coding agents. It does not implement an agent loop.

## Principles

- **The node supervises, it does not reason.** No model loop in the node. Harnesses
  are drop-in and replaceable; anything that tracks harness features will rot.
- **Enforcement over instruction.** A rule the node cannot enforce is a suggestion.
  Where enforcement is impossible, say so explicitly rather than implying uniformity.
- **Local-first.** A hub outage degrades the system. It does not stop work.
- **Nothing in project repos.** No `.claude/`, no `AGENTS.md`, no tracon files of any
  kind. Config is materialized into scratch directories at session start.
- **The corpus outlives the tooling.** Memory, docs, work items, and events use
  boring schemas with plain-text export. When the orchestration layer becomes
  obsolete, the accumulated context must survive it.

## Topology

```
                        ┌──────────────────────┐
                        │  hub (cluster)       │
                        │  always-on, relays   │
                        │  ciphertext          │
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

Every admitted node dials out to the hub; nothing accepts inbound connections. Host
location does not determine admission: a machine is a node only when it can establish
and verify the harness boundary. The hub is also the relay — both must be always-on,
and the hub already treats work channels as ciphertext-only forwarding, which is what
a relay is. What is preserved from the earlier split design: the work channel stays
opaque to the hub, the policy signing key is never on it, and channel-scoped keys
mean the relay role can be split back out if the hub's host ever becomes the less
trusted option.

## The node

A single statically linked Rust binary that supervises harnesses, speaks the mesh
protocol, serves the embedded SPA, evaluates policy locally, brokers credentials and
tools, and persists everything to local SQLite. Explicitly not its responsibilities:
model inference, an agent loop, prompt construction beyond assembling injected
context, and any business domain.

Build rules: static musl, `x86_64` and `aarch64`, SPA embedded so every node serves
the identical bundle, log to stdout, no self-daemonizing, clean SIGTERM, idempotent
restart. Supervision is external — the platform's, or the desktop wrapper's. Static
musl is also why consulta stays a Python sidecar: every Rust Oracle crate wraps a
dynamically linked glibc blob, and `oracledb` thin mode is a pure-Python wire
implementation. Recorded so the rewrite is not re-decided.

## Harness control

**ACP is the adapter interface**; Claude Code's stream-json control protocol is the
one non-ACP adapter, functionally equivalent at the permission gate. The adapter
trait is the part of the system that rots: version-pin harnesses per node, record the
version on every session, check compatibility at session start, and let an unknown
harness id refuse to start rather than fall back. A harness declares its own state
layout and config files; nothing outside the trait may spell a harness's name.

Rules learned against real harnesses, kept as rules:

- **Deny-by-default is correct.** A permission request the client cannot answer is
  denied, not silently approved. This is how local-first and fail-closed coexist:
  policy runs on the node, so auto-allowed work continues through a hub outage while
  anything needing a human blocks. Degraded means slower, never silently permissive.
- The node declares ACP filesystem reads unavailable, so the harness reads inside its
  runner rather than turning the node into a file server.
- Budget accounting includes the large, mostly cached startup context, not just the
  visible prompt.
- opencode's ACP mode starts an HTTP server and can advertise over mDNS: bind
  loopback, disable mDNS, and read the recorded cautions before adapting it.

### Model auth

**Model credentials are brokered like every other credential.** The harness holds
only a placeholder session token and reaches every provider through the node's
gateway, which injects the real credential by header — no TLS interception, the
harness's own request forwarded with its own shape, which is what subscription OAuth
tokens require. The gateway is therefore the enforcement point for provider bindings
(fail closed) and the counting point for per-channel ceilings: usage is measured
where it happens, never reported by the harness.

**The node does not implement the vendors' OAuth.** Subscription flows are the
harnesses' own clients — undocumented and churn-prone — so the node runs the
harness's login and refresh as owned subprocesses inside the boundary, surfaces the
URL and paste-back through the interface, and lifts the resulting token into its own
sealed store. The vendor logic stays in the vendor's binary; the node owns everything
around it.

## The gate

### Privilege boundary

The gate only works if the harness cannot reach what the node holds. **Every node
establishes this boundary or does not run harnesses.** There is no advisory mode: a
node that cannot enforce must not quietly run credential-adjacent work with a label
on it. Refusal is the honest state, verified at startup, and a refusing node still
serves its interface and relays so the refusal is visible from anywhere.

The boundary is capability-driven, not tied to an OS or product. Two
implementations exist — rootless Podman (internal network with no DNS, a gateway
container carrying an allowlist CONNECT proxy and the node forward, the harness
container capability-dropped with the node holding its exec pipe) and Kubernetes
(one harness pod per session, a NetworkPolicy making the node the harness's only
route, the node serving the allowlist proxy itself). Both answer the same checks
behind the same seam. The harness reaches exactly two things: allowlisted provider
hosts through the proxy, and the node through the forward.

Three things collapse the boundary and must be verified per environment:

1. **Runtime control in the harness.** A container-runtime socket or
   Docker-in-Docker gives the harness control of the gate.
2. **A shared privilege domain.** The harness must be unprivileged and isolated from
   the node; root in the node's own container fails the boundary even with no socket
   mounted.
3. **Direct egress.** Image build may use network; execution may not.

**The checks verify what runs.** They render the same run specification a session
uses onto a probe that is created but never started, so a mutating admission webhook
that adds privilege fails the check rather than passing the rendering, and a second
description of a session cannot drift from the first. `--deep` probes from inside:
no direct egress, an allowlisted host reachable, an unlisted host refused.

Contact-derived rules that stay rules: a bind mount over a symlink does not mask it,
so the harness state directory is built empty and only named files are mounted in —
nothing of the operator's leaks into a session, the same commitment as "nothing in
project repos" applied in the other direction. On a Podman machine the node forward
is TCP through the VM; on Linux it is a Unix socket and the gateway runs
`label=disable` so the harness never touches the socket while keeping its own
confinement.

### Credential classes

Two classes, and conflating them is the most likely way to build a gate that is
theater.

| Class | Held by | Notes |
|---|---|---|
| Identity / mesh keys | Node, generated locally, never transmitted | Enrollment via short-lived code from an enrolled node |
| Action credentials | Node broker, sealed, harness has no read path | Forge tokens, deploy creds, database credentials |
| Model auth | Same store, kinds `api_key` / `oauth` | Injected by the gateway; the harness holds a placeholder |

If action credentials sit on the same UID as the harness, the agent has Bash and can
read them. That is not hypothetical — it is the exact shape the broker was built to
fix.

Broker invariants:

- **No accessor returns a secret a response could carry.** The ways out are
  environment for a node-spawned subprocess, header injection inside the gateway
  module, and sealed handoff to another member. Listings carry names, kinds,
  bindings, and env key names only.
- **A credential binds to channels and to nodes.** Empty `channels` means no channel
  — unbound is unusable, not universal. Empty `nodes` means the node holding the
  file; a list pins further, so a copied store brokers nothing elsewhere.
- **The receiver's pin is the rule.** A handed-off credential not pinned to the
  receiver is dropped; the sender's bindings are a claim.
- **Widening a pin is explicit.** The CLI refuses to share a credential whose
  `nodes` does not already list the target. The authenticated interface may widen
  the pin — that is the operator deciding — and an empty pin gains the sharing node
  too, so sharing never locks the sharer out.
- **Forge tokens reach git through the environment only** — an inline credential
  helper fed by variables — never argv, never `.git/config`, never a remote URL, so
  neither `ps` nor the worktree the harness receives ever holds one.

### Brokered tools

What the agent needs are not CLIs to wrap but verbs the node exposes over MCP: the
credential never leaves the node, the harness never sees a process, and channel
bindings apply because the tool is the node's. Verbs are chosen so that **the
working agreements are the absence of a verb**: merge, transition, and deploy are
not tools, and an agent cannot call what does not exist. Database access is a
read-only SQL runner guarded twice, on both sides of the privilege boundary.

**Policy decides every tool call before the broker is touched.** An allow rule names
the tool exactly; a deny returns its reason to the agent; a tool the bundle does not
mention is put to the operator. Adding a tool never widens what runs unattended. A
missing, malformed, or badly signed bundle yields no rules, and no rules means every
request is asked: the failure mode of broken policy is more questions, never fewer.
Bundles are signed with a key never present on the hub, so a compromised hub can
serve stale policy but not new policy.

### Review

**Review the diff with a session that never saw the implementation.** A model that
watched itself reason toward a design will rationalize it; a fresh session given
only requirements and diff will not. The agent has no forge token and never runs the
publishing CLI: the node captures the diff from the worktree itself, runs the
project's checks in a throwaway container with no credentials and no tools, and the
approved bytes are the only bytes that can be posted — each file's blob hash is
recorded at submit, and approval of a branch that moved is refused naming the files.
Diff size is capped at submit, because complexity accretes when nothing says no at
submission time. An operator's hand-edit travels back as a request for changes: the
agent applies it and resubmits, and **the agent remains the only writer to the
worktree.**

## Workspaces

Sessions run in a git worktree, never a main checkout, created outside the repo from
`origin/<default>` after a fetch. A dirty main checkout is left alone and reported.
Harness config is materialized into scratch and passed explicitly, so instruction-file
discovery by directory walk stops mattering.

## Channels

A **channel** is the unit of separation: a key, not a row filter. A node not handed
the key cannot read the channel, and a meshed node refuses to start a session on a
channel it holds no key for. Channels carry bindings — which nodes may run them,
which provider, whether they notify, budgets and ceilings, which brokered tools —
enforced at the node, failing closed: a node asked to process a channel it is not
bound to refuses rather than falling back to a default. Bindings are recorded as
data, so "where did this content go" is a query rather than a recollection.

## The hub

A peer with a role, not a service tier: relay (routes ciphertext frames, keyed per
channel) and replica/processor (ordering, the nightly batch, for channels it was
explicitly handed). Both halves are one binary; the relay half never needs plaintext
and the work channel never gives it any.

**The hub is authoritative for ordering, never for availability.** Every node holds
a full local replica; reads resolve locally; sessions start with the hub
unreachable. Sync is hub-and-spoke so conflicts are only ever pairwise: site-stamped
writes, hub-assigned global order, HLC last-write-wins per record, tombstones for
deletes. Distributed SQLite options were evaluated and rejected — they all assume a
sync layer that can read the data, which the hub cannot for work channels.

**Trust asymmetry is resolved by key possession, not policy.** The cluster is
rented infrastructure, so the hub is the highest-value target and least-controlled
box at once. Channels the operator shares with it, it opens; a channel never shared
stays ciphertext. Opaque is the absence of a key, not a flag.

**Vectors are not a safe form of encrypted content.** Embedding inversion recovers
source text, so the vector index never replicates: each node embeds its own replica
and searches locally. A hub outage costs the other nodes' writes, not the ability to
search meaningfully. The index is derived state — delete it and it rebuilds.

### Mesh frames

The wire contract is `spec/README.md`, pinned by `proto/` with test vectors. A frame
is sealed then signed; every receiver verifies before opening, so a tampered frame
drops without the decrypt path running. The channel and epoch (or sender and
recipient) are bound into the AEAD associated data, so a re-labeled frame fails to
open. Direct sealing is how enrollment handoffs, credential handoffs, policy
bundles, and commands travel. Replay is closed twice (the hub remembers request
signatures, nodes remember frame ids); rekeying is a new epoch handed to the
still-trusted members, with old epochs kept so retained frames open.

Invariants on top of the wire:

- **A node speaks only for itself.** Mirrored rows naming another node than the
  verified sender are dropped and counted.
- **Receiver-owned presence.** `is_self`, `reachable`, and last-seen are set by the
  receiver, never believed from the sender.
- **Commands execute as local requests.** A command for a session or provider
  another node owns is sealed to the owner and runs there under the owner's own
  policy, store, and subprocesses; only the ack travels back. A provider login's
  subprocess, its stdin, and the lifted credential never leave the owner.
- **The hello may carry state, not secrets.** A node's models and its provider
  summary (names, states, identities — and a pending login URL) ride the hello,
  sealed to the operator's own mesh. Credential values never do; those move only as
  direct-sealed handoffs.
- One contract version, checked at enrollment. A peer on an older build drops
  payloads it cannot read; senders surface that as "unreachable, or on an older
  build" rather than silence.

## Memory and documents

The node owns memory; harness-native memory is disabled. **Bank identity comes from
the channel and project id in the mesh, never from cwd or git root** — ephemeral
worktrees at varying paths are exactly the case filesystem-anchored memory
fragments on.

Two axes: scope (global, client, project, session) and kind. Directives are
human-only and always injected; facts inject at high confidence; lessons inject only
after promotion through the approval queue, batched nightly — curation is the
difference between a memory system and a landfill, and it is the cheapest reuse of
the gate. Hard token cap on injection: oversized context degrades the session it was
meant to help.

Retrieval is FTS first — the highest-value lookups are exact — with vectors fused in
where a node is given an embedding endpoint, and the vector contribution bounded
below one tier step, because "directives above facts" is a decision about whose
instructions win, not a relevance heuristic. The embedder is a named endpoint, not a
linked-in model: that is what lets a work channel point at a local server and leave
nothing, while another channel goes through the gateway where its bindings and
ceiling still apply. Pin the embedding model and record it on every vector row.

Documents are the same store, separate table, different lifecycle: long, named,
edited as artifacts, chunked on headings for retrieval but always returned with
their slug. Memories point at documents — a twenty-token pointer beats an injected
page. Session orientation is assembled per session from the corpus, the node's own
facts, and the policy's deny reasons, delivered as a read-only file and recorded as
an event, so the transcript shows what the agent was told.

## Work ledger

Beads-inspired, not Beads: ready-work as a deterministic topological order every
replica computes identically, hash ids that two nodes mint offline without
collision, and a `discovered-from` edge so work found mid-session survives the
session. Storage is the node's SQLite, replicated per channel; readiness is derived,
never stored.

**The advantage over asking nicely is structural enforcement.** The node owns the
harness lifecycle, so it requires a work item to open a session, gates execute on
the plan artifact existing, and ends the session at item close. Context rot is
mitigated by mechanism, not by a line in a markdown file.

## Sessions, phases, budgets

Plan, execute, and review are **separate sessions the node spawns**, not phases
inside one — which sidesteps subagent model inheritance entirely, since each phase
gets an explicit model and there is nothing to inherit. A spec with no model is a
validation failure at spawn.

Budgets are denominated in tokens (dollars are derived where a provider binding
carries a price) and enforced by killing the session, checked at turn end because
that is when harnesses report usage — a property of the protocol stated honestly in
the interface rather than papered over. Channels carry daily ceilings enforced at
two points: session start is refused, and the gateway refuses the model calls of
sessions already running, so a running session stops spending and the operator
decides.

Anything checkable deterministically is checked deterministically, between phases,
in a container with no credentials. Model supervision is reserved for judgment with
no test: a cheap model watching an expensive one mostly pays twice to learn what
the test suite would have reported.

## Metrics

Event log, append-only, replicated. Human time is claim/release on approval items —
a browser heartbeat measures tab focus, not attention. Durations are recorded
monotonically on the node that observed them; never subtract timestamps across
machines. The numbers that matter at six months: approvals per accepted change,
tokens per accepted change. Provenance — which model, which prompts, which approval,
which policy version shipped a commit — is a query over data captured for metrics
anyway.

## Clients

Every node serves the same embedded SPA; a client is a matter of shell.

- **No session state in the shell.** Sessions live in the node; a client crash is a
  reconnect, never lost work. Unsent prompt drafts are held by the node per session.
  The one exception is an in-progress diff edit, confined to the desktop browser's
  local storage — the surface least likely to be evicted.
- **The phone is not a node.** No replica, no keys at rest. It reaches a node
  directly over HTTPS with a session cookie; the hub never talks to a browser. A
  backgrounded PWA cannot hold a socket, so notifications are Web Push — sealed to
  the phone's own key, sent by every node the phone subscribed at, deduplicated by
  tag. Pushes are hints; the queue is the truth.
- The service worker caches the shell and never `/api`: a cached queue you cannot
  act on is worse than an honest "cannot reach the node".
- The desktop wrapper is a tray client and nothing more — it holds no session state,
  supervises nothing but the node process it optionally runs, and adopts a node
  already answering rather than starting a second over the same state.
- The interface talks only to the node that served it; that node mirrors peers and
  forwards commands to owners. A verdict executes on the owner, because staleness
  and publishing need the owner's worktree and broker.

## Reaching a node

Three surfaces, three answers, and the differences are the point.

- **The harness** gets a separate listener with a per-session bearer token and no
  operator API: a harness that finds the forward cannot drive sessions.
- **The operator on the node's own machine** is the operator by definition:
  loopback is answered with no credential, guarded only against DNS rebinding
  (Origin and Host must be local).
- **The operator from anywhere else** needs the token, exchanged for an HttpOnly,
  Secure, SameSite=Lax cookie — a cookie because `EventSource` cannot set headers,
  Lax because a push notification's deep link is a top-level cross-site navigation.
  Only hashes of the token and of each cookie are stored, so reading the database
  mints nothing. Reissuing the token revokes every client in the same transaction.
  Secure means HTTPS is a requirement, not a recommendation, and the interface says
  so rather than looping. A login link may carry the token in the URL fragment —
  never the query string — because fragments are not sent in requests and the app
  strips them before anything renders.

## Constraints

- **Single user.** No tenancy in the auth model; one human holds keys. Channels are
  cryptographic separation of one operator's contexts. Recorded so nobody, the
  author in a year included, retrofits multi-user onto it.
- **No business domain.** Clients, invoicing, and time billing live elsewhere.
- **Bootstrap.** The node is developed by agents running inside it, so a documented
  path to running a harness outside the system is maintained, and rebuilding or
  restarting the node is a host-side recipe, never something a session performs.

## Data lifecycle

Encrypted snapshots of the hub's volume to object storage, with a restore path that
has been exercised — hub failure without a tested restore costs years of context.
Retention per kind and a real delete that propagates; tombstone semantics are
decided before there is data. Plain-text export for every kind: no format readable
only by this binary.

## Open questions

1. **Retention and tombstone semantics.** Needs deciding before data accumulates.
2. **Stacked MR handling.** Whether the node owns restacking or feature flags on
   trunk make stacks unnecessary. The trunk setup favors flags.

## What rots and what does not

The parts of this system that will rot are the harness adapters and anything
tracking harness features. The parts that keep paying are the credential broker, the
brokered tools, the review gate, the corpus, and the ledger. Build so that when the
orchestration layer becomes obsolete, the corpus and the ledger outlive it.
