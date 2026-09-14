# Roadmap

## Direction

- Personal agent workspace; one node is a complete installation. Mesh is optional.
- Enforce granted authority, not mandatory work items, phases, models, or review.
- Keep untrusted execution isolated and credentials outside agent-owned state.
- Prefer useful environments, clear evidence, and human intervention over more machinery.
- Record scoped decisions and supersede them explicitly; keep harnesses replaceable.
- Own the data: portable, independently readable exports; no node required to inspect them.
- Optimize useful work per interruption, not agent count; support independent installations,
  not multi-user tenancy.
- Keep the core accountable and personal workflows adaptable. Customizations reuse
  isolation, scoped authority, manifests and evidence; installing code grants no permission.

Completed items are removed from this file when they land; the changelog and the
reference documents under `docs/reference/` carry the history.

## OpenCode migration

**Decided 2026-09-13, cut over 2026-09-14.** OpenCode is the primary managed harness,
driven through its native server API, with its native web UI offered as an optional
advanced view. Claude Code is retained as a second supported harness and as the
Anthropic subscription login client (`claude setup-token`). omp is gone — adapter,
image, ACP layer, provider wiring and catalogue workarounds — and its sessions are
archived read-only. No new agent loop was added; the isolated execution boundary is
unchanged.

Pinned candidate: OpenCode v1.18.30, inventoried in `docs/reference/opencode-v1.18.30/`
— compatibility manifest and twenty numbered findings, route/provider/config matrices,
the minimum launch environment, the gateway deny list, and the native UI route trace.
That directory is the history and the upgrade checklist; this is the record of what
each gate delivered.

- **Gate A — candidate inventory and contract** (#189). Release pinned with checksums,
  API snapshot, server flags and startup egress verified live, provider/mutation/config
  matrices produced. Verdict: no stop condition.
- **Gate B — owner-side policy, credentials, convergence** (#190, #193–#200). One
  isolated `opencode serve` per session under a sealed launch environment (#193); the
  policy-aware API gateway — deny by default from the route matrix, directory pinned and
  bodies inspected, an all-`ask` ruleset with every `always` rewritten to `once` and the
  broadening recorded (#195); durable identity mapping and idempotent ingestion anchored
  on the per-session sequenced streams, with uncertain mutations settled by reconciliation
  (#196); per-turn usage reconciled between the gateway's on-the-wire count and the
  harness's own report, unmetered turns flagged rather than charged zero, unsent prompts
  held on the node (#197); the adversarial run and per-session state isolation behind a
  node-owned single-writer fence (#198); provider traffic proven to reach the gateway and
  only the gateway, with finding 19 — the session runner resolving a provider from the
  catalogue rather than `options.baseURL` — found and closed (#199, #200); Anthropic
  subscription login lifted out of `claude setup-token` (#190).
- **Gate C — customization and recovery** (#201–#204, #216). The node-owned launch
  manifest for skills, instructions, agents and approved plugins, with a digest recorded
  on every session (#203); LSP, formatters and the plugin cache baked into the image,
  runner egress rejected, orphaned children reaped (#202); quiesced `VACUUM INTO` backups,
  migration on a clone, generation-gated restore (#204); the direct-harness recovery route
  documented and corpus export proven to round-trip with rebuildable vectors (#201); the
  legacy transition — `tracon session archive-legacy` and `reopen` — with the cutover
  (#216).
- **Gate D — browser, desktop, and installed mobile PWA** (#205–#207, #210–#212, #217).
  The native UI served from its own origin behind a single-use bootstrap, with no password
  in the browser (#206); an unprivileged desktop window navigable only to that origin
  (#205); PTY as an explicit terminal capability with owner-bound tickets and a bounded
  proxy (#207); the installed app hosting the native view in an in-scope frame on a
  partitioned cookie (#210); the 60-shape route trace, the proof that unknown mutations
  fail closed, and the installable pinned bundle (#211, #212); and finding 20's fix — a
  node-synthesised, session-scoped, replayable `/global/event`, which is what makes the
  native view live (#217).
- **Gate E — remote-node parity** (#215). Bounded encrypted owner streams over the hub for
  HTTP, SSE and PTY: authenticated stream ids, owner binding and epoch fencing, flow
  control, reconnect without replaying input, revocation, protocol mismatch refused; the
  hub sees ciphertext and routing metadata only.
- **Gate F — release and clean cutover** (#216, #218, and this change). omp retired;
  README, architecture and design reconciled with the landed migration; the migration plan
  archived. The real Podman and Kubernetes project workflows on both harnesses remain on
  the checklist below.

Alongside the gates: an unsigned macOS bundle when Apple credentials are absent (#192),
administration and first-task workflows unified (#208), and the provider callback tests
de-raced (#194).

## Live proofs still the operator's

Everything here is built and covered by test; what is missing is a real credential, a
real device, a real cluster or a published release. Nothing on this list is a blocker for
work that does not need it, and nothing on it may be described elsewhere as proven.

- [ ] **Anthropic subscription** signed in through `claude setup-token` in the login image,
      the token lifted into the broker, and the gateway's subscription shaping serving an
      OpenCode `anthropic` provider unchanged.
- [ ] **Codex subscription** signed in through `opencode auth login openai`, and with it
      whether the ChatGPT backend accepts a Codex request whose system prompt stays in the
      message array.
- [ ] **Hosted API keys** — Anthropic and OpenAI — end to end through the gateway.
- [ ] **`OPENCODE_SERVER_PASSWORD` refused live**: an unauthenticated probe of a running
      session server, rather than of the fake.
- [ ] **The model-dependent adversarial cases**: a second identical tool call asking again
      after a gateway-mediated `once`; a subagent child session the harness raises for
      itself; and killing the server *after* a tool call was recorded.
- [ ] **Usage reconciliation against the pinned binary and a live provider**, not the fake
      server and a stub upstream.
- [ ] **A real restart against a surviving `opencode serve`**, closing the missed turn from
      the durable stream; today the container dies with the session and the path is proven
      by test.
- [ ] **The macOS desktop leg**: the bundle, the second window's behaviour, and the Edit
      menu that makes copy and paste work.
- [ ] **The desktop window against the real UI origin**, wrapper and origin and PTY
      together, rather than each against a stand-in.
- [ ] **Physical devices**: install from the always-on node over HTTPS on iOS Safari and
      Android Chrome, and confirm the framed native view authenticates with cross-site
      tracking prevention on. Safari is the open one; if the frame is refused there the
      candidate fix is `document.requestStorageAccess` from inside the frame before
      `POST /boot`, which needs a gesture and therefore a visible control.
- [ ] **The browser and PWA runs against the real pinned bundle**, which needs
      `containers/opencode-ui/build.sh` to have been run on the machine.
- [ ] **The UI bundle fetched from a published release**: `tracon setup` fetches and
      verifies `opencode-ui-v1.18.30.tar.gz`, but no release has published that asset yet,
      so only the offline `--ui-bundle` path has been exercised against real bytes.
- [ ] **The relayed run through the real hub**: `tracon session open` a session owned by
      another node, with `RUST_LOG=tracon::mesh::stream=debug` on both, confirming the
      relay's `GET /v0/streams` connection holds and `GET /v0/info` reports the same
      contract everywhere.
- [ ] **Real project workflows on both harnesses, on Podman and on Kubernetes**:
      preparation, a coding task, checks, review.
- [ ] **A private repository end to end**, through preparation, agent work, checks, and
      authorized publication to GitHub.
- [ ] **QA deploy and browser verification against a real target** (the Laravel Cloud
      preview target). Podman-only until the Kubernetes backend has a scoped QA egress
      gateway.
- [ ] **The normal workflow, proven as a workflow**: client disconnect and reconnect,
      interrupted execution, retained drafts, and recovery through completion, with what
      actually ran, what stayed uncertain, and where the operator intervened recorded.
      Fixture screenshots and fake-provider tests are not evidence of a live run.

**Upstream contributions worth a bounded PR** (not blockers): a flag that turns the
web-UI fallback into a 404; a flag check on nested instruction attachment; invoking the
declared `permission.ask` hook; an LSP status event.

## Planned

### Session portability

Move a whole session to another node, or resume it later, without losing anything the
model or the operator saw. Today the durable record is the event ledger and, for OpenCode,
the per-session state backup; the gaps are the harness's own context and the working files
a session accumulates outside the workspace.

- [ ] Save the full-text conversation before any compaction or context reset — every turn
      as the harness sent and received it — to node-owned storage keyed by session and turn,
      so a compacted context summarises something that still exists.
- [ ] Save session scratchpads with the session: scratch directories, notes and temporary
      files created outside the workspace tree, in the same package, digest and generation
      record as the state backup.
- [ ] Make that package the unit of transfer and resume: `tracon session backup` produces
      it, continuity transfer carries it, and reopening restores transcript, scratchpad,
      state and workspace checkpoint together, with lineage recorded.

### Publication follow-through

- [ ] Separate review decisions from publication recovery. Show credential and binding
      readiness before offering publication, and link missing GitHub/GitLab access to the
      right Connections settings without implying remote permission from token presence.
      A failed publish needs a durable, visible outcome and a remedy; distinguish
      definitely-not-attempted, failed, in-progress, uncertain and published, and reconcile
      uncertain external effects before retrying. Adding a credential never retries
      publication automatically, and changed revisions or prose need fresh authorization.

### Everyday work and portability

Build on the existing ledger, workspace snapshots, evidence, scoped grants and the gates
above; do not introduce another agent loop or replace the store. Order: prove ordinary
work first; then continuity, environments, authority and recovery; then reusable workflows
and grouping. More autonomy is demand-driven. This section records intended work, not
additional guarantees of the current release.

**Daily-use confidence and navigation**

- [ ] Judge changes by the existing outcome metrics — setup failures, time to verified
      work, interventions and tokens per accepted change — keeping unmetered usage and
      missing verification visible. No agent reputation score.
- [ ] Finish discoverability across surfaces: export, import, handoff and recovery
      reachable from the work they act on, and empty and archived-only states reachable on
      browser, desktop and phone.

**Node connections and desktop reliability**

- [ ] Give the Podman gateway an independent lifecycle: its own user service or cgroup,
      reconciled idempotently, so a node-service restart leaves it running and a
      stopped or missing gateway recovers without rerunning full setup or weakening
      `KillMode`. Fail-closed boundary checks stay.
- [ ] Make credential handoff status converge: after durable receiver acceptance,
      recompute provider availability and republish the local and mesh summaries.
      Distinguish queued, receiver-confirmed and usable; a successful enqueue proves
      neither receipt nor use.
- [ ] Preserve deliberate sharing through OAuth renewal: keep channel and recipient
      bindings across refresh, name one refresh owner, and never let two nodes race a
      rotating token. Removing local bindings is not evidence a copied token was revoked.
- [ ] Choose a policy for provider usage exhaustion: per project or channel, with a
      per-run override — pause and resume after reset, fall back to a named provider, or
      fall back then wait — defaulting to pause. Distinguish exhaustion from throttling,
      auth failure and outage; never invent a reset timer; record policy, reason, model
      and next wake in the ledger; recheck grants, caps and compatibility before resuming,
      and continue only from a recorded safe boundary.
- [ ] Explain browser push enrollment failures by stage — unavailable API, denied
      permission, service-worker failure, push-service registration failure, node storage
      error — without reporting a device as registered after failed enrollment.
- [ ] Open external links through a clean Linux host launcher: the AppImage's bundled
      `xdg-open` skips KDE 6 and its library path breaks a Flatpak browser. Restore the
      host environment for the child only, keep URL and origin restrictions, and never
      equate spawning a helper with opening a browser.

**Review workspace and diff reading.** Borrow interaction patterns from
[cosmicspork/review](https://github.com/cosmicspork/review), especially its
[diff viewer](https://github.com/cosmicspork/review/blob/main/src/diff-part.ts); evaluate
`diff2html`/`highlight.js` against the existing CodeMirror dependency first. Keep tracon's
immutable candidates, isolated checks and brokered publication.

- [ ] Side-by-side and unified diff modes, defaulting by width, with a remembered
      preference, aligned changed lines and synchronized scrolling; switching preserves
      file, position and feedback, and neither mode needs the desktop diff editor.
- [ ] Readable code and context: old/new line numbers, syntax highlighting, within-line
      emphasis, wrap controls, context expanded from the pinned base and candidate rather
      than the live worktree, and explicit labels for renames, binaries and missing context.
- [ ] File navigation and review progress: collapsible sections everywhere, filtering,
      per-file counts, next/previous hunk, a full-height reading view, and viewed-file
      tracking against the reviewed revision that invalidates on resubmission. Viewed is
      not approved.
- [ ] Keep large reviews responsive: headers first, incremental bodies, lazy highlighting,
      generated and oversized files collapsed with a reason and a Load diff action, bounded
      memory. Rendering performance does not waive submission caps.
- [ ] Review prose alongside code and evidence: the outgoing title and body as sanitized
      Markdown with source editing and a preview of the exact text to be published, without
      burying the diff under administrative detail.
- [ ] Persistent, anchored feedback: general and file comments, then line/range threads
      bound to revision, path and side, returned through the agent review contract and
      preserved across resubmission. Resolved and outdated are distinct; moved anchors must
      not silently attach to unrelated code.
- [ ] Durable review drafts and re-review: unsent feedback and prose edits survive
      navigation and reconnect with explicit saved/conflict state; desktop diff drafts stay
      revision-keyed; show what changed since the last reviewed revision beside the full
      base diff, and deep-link a ready-for-re-review item through the existing queue.
- [ ] Explain and validate verdict actions: keep decisions reachable in long diffs, open a
      labelled reason composer for Request changes and Reject rather than disabling a
      button, and explain genuinely unavailable actions inline.
- [ ] Prove the review experience on real surfaces: both modes in browser and desktop,
      narrow screens, long lines, mixed and generated files, light and dark, keyboard-only,
      blank reasons, missing forge credential, publication failure and retry, reconnect with
      drafts, stale revisions and resubmission into existing threads.

**Work continuity and outcomes**

- [ ] Continue the work, not just the transcript: a work-level continuation view carrying
      intent, decisions, attempts, blockers, next action, workspace, lineage and evidence,
      with continue / change approach / abandon. Plain sessions gain this without being
      forced into a work item.
- [ ] Produce a concise outcome record derived from recorded state — what changed, what was
      verified, what needs a decision, what is uncertain, and cost. Narrative summary is
      supplementary and cannot turn a claim into verification.

**Project environments and effective authority**

- [ ] Save validated project setup profiles by extending the launch manifest and toolchain
      profiles: image, language tools, skills, dependency preparation, required checks and
      configured previews, snapshotted per execution, with validation invalidated when its
      inputs change. Customizations are versioned source with provenance.
- [ ] Preview preparation and explain incompatibility before launch — detected ecosystems,
      supported steps, missing tools, remedies — without turning unsupported scripts or
      devcontainer features into silent host execution.
- [ ] Explain authority in task terms: selected node, harness and image, effective access,
      grants, spending limits and what still needs approval, derived from current policy
      rather than a parallel model. Show what a customization can read, change and send.
- [ ] Review, activate and roll back personal customizations: an agent may propose a skill,
      instruction package, profile or recipe; the operator sees source, provenance and
      validation evidence before explicit activation, which creates a revision on the
      existing manifests. Updates never change running sessions or broaden authority;
      rollback cannot undo external effects.
- [ ] Expose resource-scoped operations for custom work — evidence for a named work item,
      not ledger access; proposing a change, not mutation rights. Types describe the
      contract; the node enforces scope, and out-of-scope access is proven refused.

**Portable data and recovery**

- [ ] Publish a versioned data contract: extend the signed candidate JSON, offline package
      reader and document export into a round-trip session and corpus export — an archive of
      JSON manifest, JSON/JSONL records, Markdown and ordinary files, readable without
      tracon.
- [ ] Specify complete portable content — sessions, events, work items, decisions, drafts,
      documents, memories, workspace state, evidence and provenance, with identifiers,
      encodings and omission rules defined. Harness-native state names its compatibility and
      is never the only copy.
- [ ] Make import predictable and independently implementable: published schemas, example
      exports, integrity rules, migration policy, duplicate and reference handling, proven
      on a fresh node and by reading without one.
- [ ] Separate export, backup and handoff: export omits credentials and private keys and
      warns that transcripts can still carry secrets; backup protects deliberately included
      secrets separately; neither import nor signature verification grants authority.
- [ ] Unify recovery and maintenance entry points on the existing Settings controls: what is
      retained, how old the checkpoints are, what survives a restart, and the actions to
      inspect, export, restore or diagnose. Promise rollback only where state compatibility
      allows it.

**Cross-node session handoff**

- [ ] Move an unfinished session to another node: select destination, check compatibility,
      checkpoint, transfer and continue with continuous history and explicit lineage. No
      submitted candidate, mesh membership or work item required.
- [ ] Carry the actual continuation state — workspace changes, conversation, decisions,
      selected context, unsent draft, next action, harness/model/manifest identity,
      evidence, usage and remaining limits — over the portable contract and existing
      transport.
- [ ] Transfer execution ownership safely: quiesce and fence the source before the
      destination runs, persist acknowledgements so retries create no duplicate sessions or
      repeated side effects, and never let a timeout enable both owners.
- [ ] Re-evaluate authority and state compatibility at the destination; native-state resume
      and fresh continuation from portable context are distinct, visible outcomes, and lost
      fidelity is explained before confirmation.
- [ ] Prove handoff both directions, including interruption, unavailable destination,
      duplicate import, dirty workspace, incompatible harness state and pending external
      action, with one active owner and no budget reset.

**Reusable work and bounded automation.** Personal workflows around an accountable core,
not a new platform ([extensible software](https://jeremymorrell.dev/blog/extensible-software-in-the-age-of-llms/)).
Sequence: node and credential reliability; profiles and effective authority; reviewed
recipe authoring; one proven personal customization; then bounded triggers. Provider
fallback may be configurable, but quota classification, safe resumption, accounting and
permission enforcement stay in the core.

- [ ] Saved workflow recipes for a few recurring procedures, reusing phase presets and the
      ledger, snapshotted on invocation, revisable and stoppable. No workflow language, and
      no ceremony for a plain prompt. An agent may draft one; the operator activates a
      pinned revision.
- [ ] Prove one personal customization end to end: a preferred review summary produced from
      an approved recipe and scoped evidence, through existing report and document surfaces,
      shown to fail safely and to be revisable, disableable and exportable without
      transferring authority. Custom UI starts as isolated reports, not plugins in the
      administration UI.
- [ ] Lightweight batches: related work with dependencies, explicit ownership and reclaim
      rules, aggregate spending, blockers and one completion report, built on current
      readiness and session holders.
- [ ] Integration when parallel same-repository work needs it: prefer a forge merge queue,
      serialize, pin the combined revision and reverify — independently approved patches do
      not approve their combination. Demand-gated.
- [ ] Bounded scheduled or event-driven chores, after daily-use reliability: low-stakes work
      that returns a report and stops, with concurrency, spend, time and retry limits,
      deduplicated triggers, and approved recipe revisions run through the existing
      supervisor rather than extension code inside the node.
- [ ] Optional conversational coordination, only if useful: an ordinary session inspecting
      permitted cross-project work and proposing actions through existing tools. No
      privileged manager loop.

**Independent installations**

- [ ] Make the personal workflow reproducible by another operator: one proven
      installation-to-first-task path, explicit OS/runtime/harness/login compatibility, and
      actionable diagnostics. Remote access and mesh stay optional.
- [ ] Bound maintenance expectations: supported versus experimental integrations, pinned
      release and update policy, backup requirements, and the recovery path for a failed
      upgrade — applied to installed customizations as well as the runtime. No promise of
      arbitrary drop-in harnesses or a maintained native UI fork.
- [ ] Offer a clearly labelled demonstration mode reusing the fixture machinery, with
      demonstration data unmistakable and no implication that its output proves a live run.

## Current limitations

- Everything under "Live proofs still the operator's" is unproven live, including the
  private-repository run and QA verification against a real target.
- macOS releases are unsigned by choice — the publisher holds no Apple Developer ID — and
  are authenticated by GitHub build provenance instead, so Gatekeeper asks once on first
  open. The workflow signs and notarizes if credentials are ever configured.
- The Kubernetes runtime backend has no scoped QA browser egress gateway; `scope_qa_egress`
  always refuses there, so browser verification is Podman-only.
- The Kubernetes backend **drops** denied egress rather than refusing it. A NetworkPolicy
  has no reject verb and no portable CNI option turns a drop into an ICMP or RST refusal,
  so what Podman gets from having no route out has to come from the cluster. Until one that
  can express it is configured, a harness pod's protection against the hang is the download
  flag and the explicitly disabled server list alone — which cover OpenCode but not a future
  harness with its own timeout-free fetch. `check-boundary --deep` measures the same bound
  on both backends and fails the Kubernetes one if it drops.
- The OpenCode UI bundle is about 34 MiB of built assets. It is not in git: `tracon setup`
  fetches and verifies it, or `--ui-bundle` installs it offline, and the node serves it from
  disk against the tree digest in `containers/opencode-ui/DIGEST`. A tree that does not
  match is not served, and a node without the bundle serves no native view.
- The native UI is a v1 client of a server that raises its asks in the v2 permission system,
  and its only live channel is a global stream with no durable replay. Both are bridged by
  the node — the reply is respelled, the stream is synthesised per session — rather than by
  widening the matrix, so an upstream release that moves either moves the seam.
- A skill package is text. `SKILL.md`, scripts and text assets are staged into node-owned
  storage; a file that is not valid UTF-8 is refused at import, because the node carries the
  contents rather than a reference to them.
- Nothing registers a child session with the harness, so a fork or background subagent is
  recorded with its lineage and surfaced as `untracked` rather than driven.
- A terminal is not a per-command ledger. What a PTY records is that one was opened, with
  what shell and where, plus byte counts and duration; output capture exists, is off, and is
  labelled.
- OpenCode's v2 surface reports `cost` as zero, so a priced turn is only as good as the v1
  usage numbers the adapter reads; the gateway's on-the-wire count remains authoritative.
- The OpenCode harness image is about 1 GB, roughly a third of it the `librustc_driver` and
  LLVM shared objects rust-analyzer and rustfmt link against. The dist channel publishes no
  self-contained build of either, and a rustfmt from anywhere else formats differently from
  the workspace's pinned one.

## Deferred

- **Retrieval reranker:** when existing retrieval proves insufficient.
- **Client terminal:** superseded by the OpenCode PTY capability; no separate tracon
  terminal.
- **`cr-sqlite`:** only if real multi-writer convergence requires it.
- **Stacked MR automation:** decide whether stacks are preferable to feature flags first.
- **General dashboard/plugin system:** only after isolated reports and views prove
  insufficient. No new extension runtime, marketplace or workflow language is required for
  the personal-customization work above.

## Out of scope

- Multi-user tenancy or a team product.
- Federated work marketplaces, reputation scores or agent organization charts as product goals.
- A model/agent loop inside the node.
- A general-purpose multi-harness framework: two concrete adapters behind one trait, not a
  plugin system for harnesses.
- Forking and maintaining the OpenCode UI.
- Required tracon configuration files installed into project repositories.
- A full IDE, general file editor, or per-project editor configuration.
- Business-domain features such as invoicing and billing.
- Arbitrary host execution through the node API. Service/CLI installation and node restarts
  remain explicit desktop/CLI operations; file imports use a picker/upload flow.
