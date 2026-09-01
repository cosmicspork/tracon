# Roadmap

What is still to be built, what is deliberately not scheduled, and what will never
be. The shipped record — phases 0 through 8, each with its full checklist — lives in
[reference/shipped-phases.md](reference/shipped-phases.md), and what each phase
taught in [reference/](reference/).

Scope discipline matters more here than anywhere else. This is six products (control
plane, runner fleet, credential broker, memory service, work ledger, clients) and the
failure mode is building all six halfway. Nothing is retired until its replacement
has been in real use.

## Shipped

| Phase | | Record |
|---|---|---|
| 0 | The spikes: ACP, restricted-harness behavior, one hand-built boundary. The assumed Coder topology failed and set Phase 3's requirement. | [record](reference/shipped-phases.md) |
| 1 | Node and gate on one machine: sessions, queue, review, boundary check, policy. | [notes](reference/phase-1-spikes.md) |
| 2 | Mesh: hub relay, enrollment, mirrored sessions, cross-node control. | [notes](reference/phase-2-notes.md) |
| 3 | The work node enforces: pod-per-session Kubernetes boundary, brokered tools. | [notes](reference/phase-3-notes.md) |
| 4 | Memory, documents, provider connect, the hub's replica. | [notes](reference/phase-4-notes.md) |
| 5 | Ledger and phases: plan → execute → review, checks at submit, caps, ceilings, metrics. | [notes](reference/phase-5-notes.md) |
| 6 | Clients: PWA, operator auth, tray wrapper, diff editing, the phone's direct HTTPS path. | [notes](reference/phase-6-notes.md) |
| 7 | A second harness (Claude Code) and retrieval by meaning. | [notes](reference/phase-7-notes.md) |
| 8 | Standing on its own: release binaries, built-in push, a test suite that cannot touch real state. | [notes](reference/phase-8-notes.md) |

Since Phase 8, outside the phase structure: provisioning from anywhere (QR login,
one-line enroll bootstrap, peer provider management and credential share over the
mesh, wire contract 3), forge-backed repository listing and managed clones, and the
interface's fixture mode for screenshots.

## To build

Ordered by value. An item leaves this list by shipping or by moving, with a reason,
to Deferred or Never.

1. **Continue a work item on another node.** The ledger and plan documents already
   replicate; a session's worktree and harness do not, and never will — a "move" is
   ending the session here and starting the item's next phase there. What is missing
   is the affordance: a one-tap "continue on <node>" that ends the session cleanly
   and prefills the new-session form on the chosen node, and a session header that
   says when the item's history spans nodes.
2. **Hub-side rollups.** The hub's replica can already index shared channels;
   nothing yet summarizes a day's work across nodes into one digest. The nightly
   batch is the natural place.
3. **A signed desktop app.** The wrapper ships unsigned; macOS wants a right-click
   → Open on the first install and Windows is not a target. The AppImage and the
   macOS app already self-update from GitHub Releases after matching GitHub's
   SHA-256 digest; package-managed `.deb` installs deliberately do not. Signing
   remains distribution work, not code — what it would buy is provenance the
   digest cannot give: today a release the repository can publish is a release
   every install will take.
4. **Forge listing beyond the first page.** The repository browse reads one page
   (100 repositories, most recently active first). Enough until someone's forge is
   not; pagination is additive to the same endpoint.

## Deferred

Not rejected, not scheduled.

- **A third harness adapter** (opencode). The trait exists from Phase 1; Claude
  Code's adapter landed in Phase 7, and opencode's lands when a task needs it. Its
  ACP mode starts an HTTP server and advertises over mDNS, so it needs the recorded
  cautions read rather than a copy of either existing adapter.
- **A reranker over retrieval.** FTS and vectors together were enough for a corpus
  this size; a reranker waits for the same kind of evidence vectors did.
- **Terminal in the client.** Add only when something concrete cannot be done
  without it. `xterm.js` over the existing session channel, deliberately minimal.
- **`cr-sqlite`.** Only if real multi-writer convergence becomes necessary. The sync
  design keeps the changeset shape so this stays available.
- **Stacked MR automation.** Resolve the flags-vs-stacks question first. The trunk
  plus semantic-release setup favors feature flags, which also produces smaller
  reviews.

## Never

Out of scope by decision, recorded so it is not rediscovered as a good idea.

- **Multi-user.** No tenancy in the auth model. Single operator by design. Channel
  separation is cryptographic isolation between one operator's contexts, not tenancy.
- **An agent loop in the node.** Harnesses are replaceable; a harness written here
  is not. Owning the harness is a large undertaking whose main return is
  self-controlled churn.
- **Anything written into project repos.** No `.claude/`, no `AGENTS.md`, no tracon
  files of any kind. Config is materialized into scratch directories at session
  start.
- **A full editor.** Diff editing only. The moment a file tree, LSP wiring, and
  per-project editor preferences appear, this has become a second project.
  Configuring the *node* — its boundary, harness, credentials, channels and
  access — is not that, and belongs in the interface: an operator should never
  need a shell to stand a node up.
- **Business domain in tracon.** Clients, invoicing, and time billing stay
  elsewhere.

## Risk register

| Risk | Mitigation |
|---|---|
| Harness protocol drift | Version pinning, recorded per session, compatibility check at session start |
| Harness fights the restriction | Phase 0 drove a task under restriction before the broker was designed around cooperation |
| Scope creep into an IDE | The "Never" list above; terminal deferred by default |
| Hub becomes load-bearing | Degraded mode specified and tested, not assumed. Relay outage: auto-allow continues, approvals block |
| Hub compromise | Work channel opaque to it, policy signing key never on it, per-client keys |
| Bootstrap lockout | Documented path to running a harness outside the system, maintained |
| A node cannot establish its boundary | Every host must pass the startup check |
| Corpus lock-in | Boring schemas, plain-text export for every kind; vectors are derived, node-local, rebuildable |
| Cost runaway | Per-session budget and per-channel daily ceiling, enforced not monitored |
| Wire contract skew between nodes | One contract version, checked at enrollment; a peer on an older build drops what it cannot read and the sender is told, not silent |
