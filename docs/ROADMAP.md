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
- Support the product engineer's scarce work: understand the customer, define good,
  judge the experience, and learn from the result. Delegate implementation mechanics.
- Evaluate the candidate experience against an agreed user task; the diff is supporting
  evidence, not the whole product decision. Publication is not outcome acceptance.
- Preserve low-ceremony prompts. Product briefs, evaluation and follow-through are optional
  extensions of existing work and documents, not mandatory process or a new platform.

This priority shift follows the product-engineer definition in
[We are all Product Engineers now](https://seldo.com/posts/we-are-all-product-engineers-now/).
It changes what to build next, not what the current release already guarantees.

Completed items are removed from this file when they land; the changelog and the
reference documents under `docs/reference/` carry the history.

## Planned

Delivery order: fix trust and attention defects; prove one loop from customer context to
accepted outcome; strengthen reliability and portable ownership; then invest in richer diff
reading and optional automation. Pull forward infrastructure fixes needed by that real
workflow, rather than waiting for every platform or integration to be complete.

Build on the existing ledger, documents, workspace snapshots, evidence, scoped grants
and launch manifests. Do not replace the store or introduce another agent loop. These
are intended changes, including a future revision of the diff-first design principle,
not additional guarantees of the current release. Live validation of existing features
remains separately tracked under [Live proofs still the operator's](#live-proofs-still-the-operators).

### 1. Trust and attention

- [ ] Bind every approve, reject and request-changes verdict to the exact review revision
      the operator inspected, not only its commit SHA. Compare that identity atomically
      when recording the decision, locally and across forwarded requests. Test an old
      tab after a same-SHA resubmission with changed requirements or prose; require fresh
      review. Unchanged code may reuse checks, never an unseen human decision.
- [ ] Reserve orientation space for current intent, constraints and operator directives
      before discretionary guides and background context. Name omitted or truncated
      context. Test multiple long guides without losing the task, plan's essential
      constraints or directives; a generic trimmed flag is not sufficient.
- [ ] Normalize managed-harness permissions into canonical action, kind and resource or
      command independently of display titles. Prove actual OpenCode adapter-shaped read
      events pass through the supervisor and receive the shipped read exemptions; unknown
      capabilities remain asked or denied. Keep every harness call node-mediated and
      never substitute a blanket harness-level always-allow grant.
- [ ] Make the attention count mean actionable human decisions. Separate needs-your-decision,
      agent-revising and external-operation/recovery states without losing their durable
      threads. Show work intent and session links on permission cards; preserve explicit
      expiry and deny-by-default behavior rather than treating deferral as consent.
- [ ] Explain authority in task terms: node, harness, image, effective access, grants,
      limits and what still needs approval, derived from current policy, not a parallel
      model. Reduce low-value interruptions without broadening external-write authority.

### 2. One complete product loop

The first product slice is one real user problem carried through implementation,
an evaluable candidate, specific feedback, a revised attempt and explicit acceptance.
It must also preserve what was learned after the intended user encountered the result.

**Customer context and definition of good**

- [ ] Extend existing work and documents with an optional product brief: intended user,
      problem, source references, constraints, success criteria and unresolved questions.
      Distinguish customer observations, agent inferences and operator decisions; connect
      evidence to the problem and chosen approach without requiring a research database.
- [ ] Let the operator select a versioned, work-specific context bundle that follows
      subsequent attempts: brief, research, decisions, constraints and relevant documents.
      Record what each attempt actually received and make changes or omissions visible;
      channel-wide guides and memory retrieval are not a substitute for selected context.
- [ ] Link individual acceptance criteria to checks, scenarios, observations and human
      verdicts. Make uncovered criteria, assumptions and unresolved questions visible.
      An agent may propose what good means; passing its own checks does not establish
      that the customer agreed with the standard.

**Experience-centered evaluation**

- [ ] Make the candidate experience against its agreed user task the primary product-review
      surface: task, criteria, exact preview, evidence, observations and verdict together.
      Keep the diff and runtime checks available as supporting evidence. Update the design
      record when this lands, without removing implementation review or publication gates.
- [ ] Save inspectable, versioned customer-task scenarios with starting conditions,
      observable outcomes and device/viewport settings. Let agents propose executable
      steps; retain the exact scenario with each run and rerun it against later candidates.
      Separate changes to the evaluation standard from changes to the implementation.
- [ ] Support an operator-led trial of the exact deployed candidate with deployment identity
      and freshness visible. Capture observations against the candidate and UI state;
      static prototypes must state their fidelity and keep their isolated sandbox.
      Screenshots prove capture, not usability; distinguish automated assertions from
      criteria awaiting human judgment, including narrow-screen behavior.

#### Publication follow-through

- [ ] Separate authorization to publish, technical verification and human acceptance of the
      outcome. Support optional published/awaiting-evaluation work rather than closing it
      merely because a PR opened. Bind acceptance to the evaluated candidate and criteria;
      a new candidate must not inherit an old verdict. Plain coding tasks may still end at
      publication when that is the operator's chosen endpoint.
- [ ] Separate review decisions from publication recovery. Show credential and binding
      readiness before offering publication and link missing forge access to the right
      settings, without implying remote permission from token presence. A failed publish
      needs a durable, visible outcome and a remedy: distinguish definitely-not-attempted,
      failed, in-progress, uncertain and published, and reconcile uncertain external
      effects before retrying. Expose an explicit recovery action backed by the publication
      journal, not a misleading second approval. Adding a credential never retries
      automatically; changed revisions or prose need fresh authorization.

**Feedback and re-review**

- [ ] Preserve individually addressable observations anchored to criteria, screenshots or
      UI states, as well as general and file comments and line/range threads bound to
      revision, path and side. Return feedback through the agent review contract and keep
      it across resubmission. Addressed, unresolved and outdated are distinct; a moved
      anchor must not silently attach to unrelated experience or code.
- [ ] Durable review drafts: unsent feedback and publication prose survive navigation,
      reconnect and device changes with explicit saved/conflict state. Desktop diff drafts
      remain revision-keyed; preview-to-review navigation must not discard observations.
- [ ] Show before/after evidence and what changed in response to each observation, with
      changes since the last reviewed revision beside the full base diff. The operator
      should not need to reconstruct feedback from transcripts or reread the entire change
      to find the one concern that remains unresolved.
- [ ] Keep verdicts reachable in long reviews, open a labelled reason composer for Request
      changes and Reject rather than disabling a button, and explain unavailable actions
      inline. Preview and evidence navigation must lead back to the decision being made.
- [ ] Prove feedback continuity: leave three independent observations, switch device or
      reconnect, receive a revision addressing two, and find the remaining concern intact.
      Include stale requirements, changed scenarios and resubmission into existing threads.

#### Everyday work and portability

Keep intent and learning continuous within ordinary work before extending whole-session
transfer across nodes. Plain sessions gain these capabilities without requiring a ledger
item; broader export and cross-node commitments remain below.

- [ ] A work-level continuation view carrying intent, decisions, attempts, blockers, next
      action, workspace, lineage and evidence, with continue / change approach / abandon.
      Plain sessions gain it without being forced into a work item.
- [ ] A concise outcome record derived from recorded state: what changed, what was
      verified, what needs a decision, what is uncertain, and cost. Narrative summary
      cannot turn a claim into verification or publication into customer success.
- [ ] Optional post-publication follow-through: record publication, merge, deployment,
      evaluation and customer observation separately. Keep the expected outcome, an
      observation source and next observation date/action with the work. Use customer
      notes, existing database queries or external dashboard links; connect what was
      learned to continue / change approach / stop, without building an analytics platform.
- [ ] Judge tracon by fewer low-value interruptions, less context re-explanation and time
      to an evaluable experience and an accepted outcome. Retain setup failures, time to
      verified work, interventions and tokens per accepted change, explicitly labelled as
      approved/published reviews rather than human-accepted outcomes or observed customer
      benefit. Include total effort across unsuccessful attempts when measuring cost per
      accepted outcome; keep unmetered usage and missing verification visible. Quantitative
      measures supplement qualitative user judgment; no agent reputation score.

**Project readiness and independent use**

- [ ] Save validated project setup profiles by extending the launch manifest and toolchain
      profiles: image, language tools, skills, preparation, required checks and previews,
      snapshotted per execution, with validation invalidated when its inputs change.
- [ ] Preview preparation and explain incompatibility before launch, without turning
      unsupported scripts or devcontainer features into silent host execution. Let the
      agent propose project checks and setup, but require operator validation; it cannot
      waive checks or authorize its own environment.
- [ ] Distinguish ready-to-investigate, ready-to-verify and ready-to-publish. Surface missing
      project checks or publication prerequisites before spending a session on that path;
      do not require forge credentials, QA setup or a product brief for an investigation.
- [ ] One proven installation-to-first-task path for another operator, with explicit
      OS/runtime/harness/login compatibility and actionable diagnostics. Remote access and
      mesh stay optional. Demonstrate that project readiness leads to useful completion,
      not only a runnable node.

**Exit proof**

- [ ] Prove one independent operator's complete loop on a real project: capture a user's
      problem and source, agree on good, delegate with selected context, capture a candidate,
      obtain a preview, try the task, record observations, inspect a revision and explicitly
      accept the result. Record the intended user's subsequent experience and next decision.
      Include disconnect/reconnect and recovery without losing context or feedback.
- [ ] In a preview workflow that requires a PR first, publish the candidate, fail a user
      task in the preview, and show that the product outcome remains unaccepted with an
      obvious continuation path. Only the later explicit acceptance counts as acceptance.
      Reuse the private-repository and live-QA proofs below; fixtures are not substitutes.

### 3. Reliability and portable ownership

Fix prerequisites for the product loop as they arise. Broader platform parity and
cross-node transfer follow the first proven loop, not the other way around.

**Node connections and desktop reliability**

- [ ] Give the Podman gateway its own user service or cgroup, reconciled idempotently, so
      a node-service restart leaves it running and a stopped gateway recovers without
      rerunning setup or weakening `KillMode`. Fail-closed boundary checks stay.
- [ ] Converge credential handoff status: after durable receiver acceptance, recompute
      provider availability and republish the local and mesh summaries. A successful
      enqueue proves neither receipt nor use.
- [ ] Re-offer a shared credential to a holder that missed its handoff: one offline past
      hub retention (a cursor that jumps ahead on `410`) never receives the renewed copy,
      and nothing sends it again. Do not treat removed local bindings as evidence a
      copied token was revoked.
- [ ] Sync node configuration across the mesh the way credentials will be: set once and
      carried to every member, with secrets only ever in direct-sealed handoffs and
      everything else on the hub's ordered replication (last writer wins per record).
      Define which settings are mesh-wide, per channel, and per node — a node's own
      runtime, boundary and paths stay its own.
- [ ] Choose a provider-exhaustion policy, per channel with a per-run override: pause and
      resume after reset, fall back to a named provider, or fall back then wait,
      defaulting to pause. Distinguish exhaustion from throttling, auth failure and
      outage; never invent a reset timer; record policy, reason, model and next wake;
      recheck grants, caps and compatibility before resuming, and continue only from a
      recorded safe boundary.
- [ ] Explain browser push enrollment failures by stage: unavailable API, denied
      permission, service-worker failure, push-service registration failure, node storage
      error; never report a device as registered after a failed enrollment.
- [ ] Open external links through a clean Linux host launcher: the AppImage's bundled
      `xdg-open` skips KDE 6 and its library path breaks a Flatpak browser. Restore the
      host environment for the child only and keep the URL and origin restrictions.

**Portable data and recovery**

- [ ] Publish a versioned data contract: the signed candidate JSON, offline package reader
      and document export extended into a round-trip export readable without tracon.
- [ ] Specify complete portable content: sessions, events, work items, decisions, drafts,
      documents, memories, workspace state, evidence, provenance, selected product context,
      criteria and observations, with identifiers, encodings and omission rules defined.
      Harness-native state is never the only copy.
- [ ] Make import predictable and independently implementable: schemas, example exports,
      integrity rules, migration policy, duplicate and reference handling, proven on a
      fresh node and by reading without one.
- [ ] Separate export, backup and handoff: export omits credentials and private keys and
      warns that transcripts and customer context can still carry sensitive information;
      neither import nor signature verification grants authority.
- [ ] Unify recovery and maintenance entry points on the existing Settings controls, and
      promise rollback only where state compatibility allows it.
- [ ] Make export, import, handoff and recovery discoverable from the work they act on,
      and keep empty and archived-only states navigable on every surface.
- [ ] Bound maintenance expectations: supported versus experimental integrations, pinned
      release and update policy, backup requirements, and the recovery path for a failed
      upgrade, for installed customizations as well as the runtime.

#### Session portability

Move a whole session to another node, or resume it later, without losing anything the
model or the operator saw. The durable record today is the event ledger and, for
OpenCode, the per-session state backup; the gaps are the harness's own context and the
working files a session accumulates outside the workspace.

- [ ] Save the full-text conversation before any compaction or context reset: every turn
      as the harness sent and received it, keyed by session and turn, so a compacted
      context summarises something that still exists.
- [ ] Save session scratchpads with the session, in the same package, digest and
      generation record as the state backup.
- [ ] Make that package the unit of transfer and resume: backup produces it, continuity
      transfer carries it, reopening restores transcript, scratchpad, state and workspace
      checkpoint together with lineage recorded.

**Cross-node session handoff**

- [ ] Move an unfinished session to another node: select, check compatibility, checkpoint,
      transfer, continue, with continuous history and explicit lineage, requiring no
      candidate, mesh membership or work item.
- [ ] Carry the actual continuation state: workspace changes, conversation, decisions,
      context, unsent draft, next action, harness/model/manifest identity, evidence, usage
      and remaining limits, over the portable contract and existing transport.
- [ ] Transfer ownership safely: quiesce and fence the source first, persist
      acknowledgements so retries duplicate nothing, and never let a timeout enable both
      owners.
- [ ] Re-evaluate authority and state compatibility at the destination; native-state resume
      and fresh continuation are distinct, visible outcomes, and lost fidelity is explained
      before confirmation.
- [ ] Prove handoff both directions, including interruption, unavailable destination,
      duplicate import, dirty workspace and incompatible harness state, with one active
      owner and no budget reset.

### 4. Richer review and optional automation

These commitments remain, but follow a working product-evaluation loop. More agent
coordination or richer code rendering is not a substitute for better product judgment.

**Diff reading.** Borrow interaction patterns from
[cosmicspork/review](https://github.com/cosmicspork/review), especially its
[diff viewer](https://github.com/cosmicspork/review/blob/main/src/diff-part.ts); weigh
`diff2html`/`highlight.js` against the existing CodeMirror dependency first. Keep
tracon's immutable candidates, isolated checks and brokered publication.

- [ ] Side-by-side and unified modes, defaulting by width, with aligned lines, synchronized
      scrolling and a remembered preference; switching preserves file, position and
      feedback, and neither mode needs the desktop diff editor.
- [ ] Readable code and context: line numbers both sides, syntax highlighting, within-line
      emphasis, wrap controls, context expanded from the pinned base and candidate rather
      than the live worktree, and explicit labels for renames, binaries and missing context.
- [ ] File navigation and progress: collapsible sections, filtering, per-file counts,
      next/previous hunk, a full-height reading view, and viewed-file tracking against the
      reviewed revision that invalidates on resubmission. Viewed is not approved.
- [ ] Keep large reviews responsive: headers first, incremental bodies, lazy highlighting,
      generated and oversized files collapsed with a reason and a Load diff action.
      Rendering performance does not waive submission caps.
- [ ] Present the outgoing title and body as sanitized Markdown with source editing and a
      preview of the exact text to be published, without burying the evaluation or diff.
- [ ] Prove the review experience on real surfaces: both modes in browser and desktop,
      narrow screens, long lines, generated files, light and dark, keyboard-only, blank
      reasons, missing forge credential, publication failure, reconnect with drafts, stale
      revisions and resubmission into existing threads. Keep the earlier product-loop and
      feedback proofs as regression coverage, not deferred to this rendering work.

**Reusable work and bounded automation.** Personal workflows around an accountable core,
not a new platform ([extensible software](https://jeremymorrell.dev/blog/extensible-software-in-the-age-of-llms/)).
Sequence: node and credential reliability; profiles and effective authority; reviewed
recipe authoring; one proven customization; then bounded triggers. Quota classification,
safe resumption, accounting and permission enforcement stay in the core.

- [ ] Let an agent propose a skill, instruction package, profile or recipe, and show source,
      provenance and validation evidence before explicit activation, which creates a
      revision on the existing manifests. Updates never change running sessions or broaden
      authority; rollback cannot undo external effects.
- [ ] Expose resource-scoped operations for custom work: evidence for a named work item,
      not ledger access; proposing a change, not mutation rights; prove out-of-scope
      access refused.
- [ ] Saved workflow recipes for a few recurring procedures, reusing phase presets and the
      ledger, snapshotted on invocation, revisable and stoppable. No workflow language, and
      no ceremony for a plain prompt.
- [ ] Prove one personal customization end to end: a preferred review summary from an
      approved recipe and scoped evidence, shown to fail safely and to be revisable,
      disableable and exportable without transferring authority. Custom UI starts as
      isolated reports, not plugins in the administration UI.
- [ ] Lightweight batches: related work with dependencies, explicit ownership and reclaim
      rules, aggregate spending, blockers and one completion report.
- [ ] Integration when parallel same-repository work needs it: prefer a forge merge queue,
      serialize, pin the combined revision and reverify. Demand-gated.
- [ ] Bounded scheduled or event-driven chores, after daily-use reliability: low-stakes work
      that returns a report and stops, with concurrency, spend, time and retry limits, run
      through the existing supervisor rather than extension code inside the node.
- [ ] Optional conversational coordination, only if useful: an ordinary session inspecting
      permitted cross-project work and proposing actions through existing tools. No
      privileged manager loop.
- [ ] A clearly labelled demonstration mode reusing the fixture machinery, with no
      implication that its output proves a live run.

## OpenCode migration

**Decided 2026-09-13, cut over 2026-09-14.** OpenCode is the primary managed harness,
driven through its native server API, with its native web UI offered as an optional
advanced view. Claude Code is retained as a second supported harness. omp is gone — adapter,
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
- **Gate B — owner-side policy, credentials, convergence** (#190, #193, #195–#200). One
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
  archived. A QA target gained a second deployment kind so the first real run need not be
  GitLab-shaped: `command` runs an operator-configured argv on the node with a brokered
  credential as environment — never in argv, never in the recorded tail — observes the host
  through further argv, and ends in the same identity check and immutable browser binding,
  so browser verification is unchanged (#218). The real Podman and Kubernetes project
  workflows on both harnesses remain on the checklist below.

Alongside the gates: the macOS bundle ships unsigned (#192),
administration and first-task workflows unified (#208), and the provider callback tests
de-raced (#194).

## Live proofs still the operator's

Everything here is built and covered by test; what is missing is a real credential, a
real device, a real cluster or a published release. Nothing on this list is a blocker for
work that does not need it, and nothing on it may be described elsewhere as proven.

- [ ] **Anthropic subscription** signed in through the node's own OAuth flow, by local
      callback and by paste, refreshed by the refresh loop, and the gateway's subscription
      shaping serving an OpenCode `anthropic` provider unchanged. The flow's exchange,
      refresh and an inference call were confirmed by hand against the live service on
      2026-09-15; the node doing it is not yet proven.
- [ ] **Codex subscription** signed in through the node's own OAuth flow, by local callback
      and by device code, and with it whether the ChatGPT backend accepts a Codex request
      whose system prompt stays in the message array.
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
      authorized publication to GitHub. This is the same GitHub-hosted project the QA run
      below deploys, and its publication is that run's precondition: Laravel Cloud's
      automation opens the preview environment when the pull request opens, so the branch
      has to be on the forge before there is anything to discover.
- [ ] **QA deploy and browser verification against a real target** (the Laravel Cloud
      preview target). The documented recipe *discovers* rather than creates: it finds the
      preview environment Cloud's pull-request automation made for the candidate's published
      branch, waits for it, compares the deployed commit to the candidate, and leaves
      teardown to the platform. `cloud` v0.5.0 cannot deploy a bare commit, so an
      unpublished candidate is refused with that reason rather than deploying a branch head.
      Still the operator's: a Cloud API token in the broker as `laravel-cloud` (read and
      deployment-status scope), the `cloud` login written into the node-owned home, the
      target's attested `origin_suffix` and app name, an identity endpoint on the
      application that returns its build commit, and a digest-pinned browser image.
      Podman-only until the Kubernetes backend has a scoped QA egress gateway.
- [ ] **The normal workflow, proven as a workflow**: client disconnect and reconnect,
      interrupted execution, retained drafts, and recovery through completion, with what
      actually ran, what stayed uncertain, and where the operator intervened recorded.
      Fixture screenshots and fake-provider tests are not evidence of a live run.

**Upstream contributions worth a bounded PR** (not blockers): a flag that turns the
web-UI fallback into a 404; a flag check on nested instruction attachment; invoking the
declared `permission.ask` hook; an LSP status event.

## Current limitations

- Everything under "Live proofs still the operator's" is unproven live, including the
  private-repository run and QA verification against a real target.
- macOS releases are unsigned — the publisher holds no Apple Developer ID — and are
  authenticated by GitHub build provenance instead, so Gatekeeper asks once on first open
  (right-click Open, or System Settings > Privacy & Security > Open Anyway).
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
