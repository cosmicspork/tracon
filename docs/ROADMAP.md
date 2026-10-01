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
- The operator's scarce work is understanding the problem and judging the result; tracon's
  job is to take the implementation mechanics off their plate reliably. Product briefs,
  criteria and follow-through stay optional extensions of ordinary work, never process.

**Reprioritised 2026-09-29.** The 2026-09-15 turn towards product engineering added a
layer of evaluation machinery before tracon had run a real session, and the first real
runs (2026-09-20, 2026-09-28) found the everyday path broken underneath it. The plan
now is ordered by what it takes to start a session, build something and ship it from
one node, every day. The product-engineering work that shipped (the brief, selected
context, criteria bound to checks) stays and stays optional; the rest waits until its
absence is felt in real work.

### The supported configuration

One configuration is what daily use runs on, and what a defect has to be in to enter
**Now**: the desktop app on macOS or Linux, rootless Podman, Claude Code as the managed
harness, a single node. Everything else — OpenCode, the Kubernetes runtime, the hub and
mesh, the installed phone app, handoff between nodes — is built and tested but
*experimental* until that configuration is boring. The homelab node keeps running the
hub and serving the phone; work on those surfaces is deferred, not removed.

### How work enters this file

An item enters **Now** only when it was hit during real use, and says where: the session,
task or run that found it. **Next** holds work whose need is already clear from daily
use. **Later** holds intended changes that wait for the loop before them to be proven.
Nothing enters from a blog post, an idea or a demo without first being missed on a real
task. Completed items are removed when they land; the changelog and the reference
documents under `docs/reference/` carry the history. Live validation of existing
features is tracked separately under [Live proofs](#live-proofs-still-the-operators).

Build on the existing ledger, documents, workspace snapshots, evidence, scoped grants
and launch manifests. Do not replace the store or introduce another agent loop.

## Now — start, build, ship from one node

Every item here was found on the 2026-09-20, 2026-09-28 or 2026-09-30 live runs, or in
the daily desktop use since.

- [ ] **Give the session its repository's toolchain and registries.** Checks now run in
      the repository's image with its dependencies prepared, but the session itself still
      runs in the harness image and meets the egress proxy's 403: `cargo add`, `npm
      install`, `composer require`, `pip` and `go get` fail, and the agent cannot run the
      tests it will be judged by until it submits. "Add dependency Y and run the tests" and
      "update package Z" are ordinary tasks and are not possible. Three parts, in order.
      Build the repository's dev environment: its `.devcontainer` Dockerfile, built by the
      node from the default branch and never from a candidate's worktree, the digest
      recorded as the `[[repo]]` image and rebuilt when the Dockerfile's hash changes —
      only the Dockerfile is honoured; hooks, compose files and features stay refused, and
      `prepare` takes their place. Layer the harness onto that image, so a session has the
      tools its checks have. Then let a `[[repo]]` entry open its registry presets to its
      sessions, opt-in per repository, and make the refusal elsewhere say "not reachable
      from a session; add it to this repository's egress". Alongside: prepare once per
      candidate rather than before every check, keep one candidate's install from reaching
      another's evidence (the cache is per repository and image today), give the scoped
      gateway a filter per client rather than one holder at a time, and put the table in
      Settings and the CLI. Found while making required checks run, and reviewing which of the
      operator's own repositories could pass theirs (2026-09-30).
- [ ] **Proof of work.** One MCP tool that runs a command in the session's own boundary
      and has the *node* record the command, exit code, bounded output, revision and
      image digest as evidence — the agent cannot type the output. A proof document is
      assembled from those records, shown beside the diff in review, marked stale when
      the revision moves, and rerun to verify. This is the Showboat idea with the
      capture owned by the supervisor; the existing attached demonstration (a linked,
      hashed, never-executed document) stays for what a human writes by hand. Depends on
      the session toolchain above for anything that needs the project's tools.
- [ ] **A work item's session starts working on its own.** A session composed from a
      prompt, or started on an existing item, is sent no first prompt — only a plain
      prompt session carries `initial_prompt` — so it sits `running` and silent until the
      operator types something; plan session `01a0f49f` (2026-09-30) waited for a nudge.
      Send the item, or for execute a kickoff naming the plan document, as the first
      prompt, so starting work is one action.
- [ ] **Keep the machine awake while a session works.** The desktop host idle-suspended
      for 54 minutes in the middle of execute session `01a0f4b3`'s turn (2026-09-30,
      during the harness's context compaction) and the session simply stopped. Hold a
      sleep inhibitor while any turn is running or a permission is waiting (logind's
      `Inhibit` on Linux, a power assertion on macOS), release it when the node is
      idle, show in the interface when it is held, and record a suspend that happens
      anyway as an interruption rather than a silent gap.
- [ ] **Give a node restart its own end reason.** Sessions running when the node restarts
      are ended as `killed_user`, which tells the operator they stopped something they did
      not; record `node_restart` and offer the resume a restart interrupted.
- [ ] **Give the Podman gateway its own user service or cgroup**, reconciled idempotently,
      so a node-service restart leaves it running and a stopped gateway recovers without
      rerunning setup or weakening `KillMode`. Fail-closed boundary checks stay.
- [ ] **Publication recovery.** Show credential and binding readiness before offering
      publication and link missing forge access to the right settings, without implying
      remote permission from token presence. A failed publish needs a durable, visible
      outcome and a remedy: distinguish definitely-not-attempted, failed, in-progress,
      uncertain and published, and reconcile uncertain external effects before retrying.
      Expose an explicit recovery action backed by the publication journal, not a
      misleading second approval. Adding a credential never retries automatically;
      changed revisions or prose need fresh authorization.
- [ ] **Make tracon invisible in what it publishes.** A commit, branch, pull request or
      forge request should read as the operator's own work through their harness, with
      nothing naming tracon. Authorship comes from the bound forge credential, not the
      host or a placeholder: inside the boundary every commit is `tracon <tracon@localhost>`
      today (hard-coded in the harness home's gitconfig and the sanitized workspace
      config), and outside it commits carry the host's Git config, which can be a work
      address on a personal repository. Resolve the identity once per bound credential
      from the forge (GitHub `/user`: name and `<id>+<login>@users.noreply.github.com`;
      GitLab `/user`: name and commit or noreply email), cache it with the binding, and
      write it as author and committer wherever the node writes Git config. Attribution
      is see-through: the harness's own default trailers (Claude Code's `Co-Authored-By`,
      whatever OpenCode does) pass through untouched, and tracon adds none of its own;
      its provenance stays in the node's ledger. Forge API calls stop sending
      `user-agent: tracon`. For an external harness the node launches nothing, so it
      offers the identity (in `external show` and as a tool result the harness applies as
      repo-local config) and checks authorship at `submit_review` and publish: a commit
      whose author is not an identity of the target forge account is named in the review,
      and rewriting it is an operator-approved step, never silent.
- [ ] **Open external links through a clean Linux host launcher**: the AppImage's bundled
      `xdg-open` skips KDE 6 and its library path breaks a Flatpak browser. Restore the
      host environment for the child only and keep the URL and origin restrictions.

## Next — the working loop, made comfortable

Needed for daily use, but not blocking it today.

**Sessions and accounting**

- [ ] Record a policy decision for every Claude Code tool call. Calls Claude Code allows
      by its own rules (reads, searches) never reach `can_use_tool`, so they leave no
      `policy_allowed` event, while the same calls under OpenCode do; the ledger should
      not depend on which harness ran.
- [ ] Choose a provider-exhaustion policy, per channel with a per-run override: pause and
      resume after reset, fall back to a named provider, or fall back then wait,
      defaulting to pause. Distinguish exhaustion from throttling, auth failure and
      outage; never invent a reset timer; record policy, reason, model and next wake;
      recheck grants, caps and compatibility before resuming, and continue only from a
      recorded safe boundary.
- [ ] A work-level continuation view carrying intent, decisions, attempts, blockers, next
      action, workspace, lineage and evidence, with continue / change approach / abandon.
      Plain sessions gain it without being forced into a work item.
- [ ] A concise outcome record derived from recorded state: what changed, what was
      verified, what needs a decision, what is uncertain, and cost. Narrative summary
      cannot turn a claim into verification.
- [ ] Distinguish ready-to-investigate, ready-to-verify and ready-to-publish. Surface
      missing project checks or publication prerequisites before spending a session on
      that path; do not require forge credentials, QA setup or a product brief for an
      investigation.
- [ ] Preview preparation and explain incompatibility before launch, without turning
      unsupported scripts or devcontainer features into silent host execution. Let the
      agent propose project checks and setup, but require operator validation; it cannot
      waive checks or authorize its own environment.

**What the boundary defeats, and what replaces it.** The harnesses offer tools the
isolation silently breaks — the proxy answers 403, the agent sees a network error and
retries variations. Each item below is a brokered replacement on the node, shaped like
the forge and tracker tools: policy on the request, the result recorded, the harness's
own tool left useless by design and the orientation saying so. Enter **Now** as each is
hit on a real task.

- [ ] Prompt attachments: a screenshot or a log file sent with a prompt, from the phone
      too, staged into the workspace under a fixed path and named in the turn. `PromptBody`
      is text only today.
- [ ] `web_fetch` on the node: URL policy (allow and deny patterns, `kinds = ["fetch"]`), a
      size cap and a redirect limit, the URL and a digest of the body recorded as evidence.
      Claude Code's server-side `WebSearch` already works through the gateway; every
      client-side fetch, `curl` included, does not and should say why.
- [ ] `repo_fetch`: the node refreshes the workspace's remote refs with its own credential
      and records the sha, so a session can rebase onto a `main` that moved since launch.
      Inside the boundary `git fetch` has neither a credential nor a route.
- [ ] Refusals the agent can read: what the boundary refuses, and which brokered tool to
      use instead, stated in the orientation; where the node can intercept a refused egress,
      a tool-shaped message rather than a bare 403.
- [ ] Files back to the operator: an artefact a session made (a diagram, a screenshot, a
      generated report) attached to the session and downloadable from its screen, without
      committing it. Only `report_issue` carries attachments today.
- [ ] Seeing a running app: the QA browser pointed at the session's own dev server, one
      screenshot per call, recorded — the same capture the proof-of-work item wants.
- [ ] Secrets for project checks: a credential class injected into preparation and check
      runs only (a test database URL, a sandbox key), never into the session.

**Review and publication**

- [ ] What ships under the operator's name is reviewed: the branch name and the commits.
      Today the candidate is the tree, the review shows the diff and the forge prose, and
      the commits and branch are pushed as the agent wrote them (the default branch even
      names tracon). Three steps: list the commits and the branch beside the diff;
      treat commit message and branch name as publication prose like the forge
      description — proposed by the agent, edited by the operator, bound by hash under the
      publish grant — with the node squashing the candidate into one commit carrying the
      approved message on the approved branch by default (`publish.commits = squash |
      keep` per channel or repository; `keep` edits each message), which leaves the tree
      and therefore the candidate, its evidence and the verdict unchanged; and a
      deterministic subject-and-branch check (conventional type, imperative subject,
      kebab branch, no ticket keys) whose rules come from the channel or the repository's
      own guidelines, so a bad message fails before the card reaches the operator. Record
      the "reviewed tree, not reviewed bytes" reframing in ARCHITECTURE and DESIGN.
- [ ] Follow a published pull or merge request after it opens: subscribe to its CI runs,
      draft/ready and open/merged/closed transitions, review verdicts and new comments or
      threads, and record each as an event on the review and its work item. Notify through
      the existing push channel with the change named ("CI failed on `node`", "marked
      ready", "2 new comments"), and let a session that is still attached pick the change
      up through the forge tools (#297, #298) rather than the operator relaying it. Polling
      through the brokered token first; webhooks only where the node is reachable. A
      subscription ends when the request closes, and it never merges, approves or retries
      anything on its own.
- [ ] Durable review drafts: unsent feedback and publication prose survive navigation,
      reconnect and device changes with explicit saved/conflict state. Desktop diff drafts
      remain revision-keyed. The acceptance test is the reconnect: leave feedback, switch
      device or reconnect, find it intact.
- [ ] Show changes since the last reviewed revision beside the full base diff, and what
      changed in response to each comment, so the operator does not reread the entire
      change to find the one concern that remains unresolved.
- [ ] Keep verdicts reachable in long reviews, open a labelled reason composer for Request
      changes and Reject rather than disabling a button, and explain unavailable actions
      inline.
- [ ] Trim criteria binding to what is produced. #290 lets a criterion point at a
      `scenario` or an `observation`; neither is produced by anything now that versioned
      scenarios and trial capture are dropped. Remove those link kinds, keep `check` and
      the operator's judgement, and say so in the brief format.

**Forge and tracker**

- [ ] Give GitHub the CI tools GitLab has: a run's job log tail (`run_logs`, the
      `job_trace` limits), rerunning a run's failed jobs (`run_rerun`, asked), and the
      runs at an exact commit. An agent on a GitHub project should diagnose and retry CI
      the way it can on GitLab, without the operator's token or a host CLI.
- [ ] Discover Jira transitions: `issue_transitions` lists an issue's currently available
      transitions (id, name, destination status) through the brokered token, so an
      authorized `issue_transition` never rests on an out-of-band `acli` lookup or an id
      copied from another project. Discovery stays a read; the transition keeps its grant.

**Node data**

- [ ] **Data management in Settings.** Retention is decided: the node keeps everything
      and the operator deletes by hand. Give that a pane — what the node holds per kind
      (sessions, events, candidates and evidence, documents, memories, workspaces, harness
      state), how much it weighs, and delete for the kinds that have a real delete, with
      the propagation each one does or does not have stated. A storage figure belongs on
      the Nodes screen too. Tombstones stay undecided until replication makes them matter.
- [ ] Explain browser push enrollment failures by stage: unavailable API, denied
      permission, service-worker failure, push-service registration failure, node storage
      error; never report a device as registered after a failed enrollment.

**OpenCode, contingent on the spike above**

- [ ] Show an OpenCode session's history in its native window. OpenCode keeps separate v1
      and v2 session stores; the v2 runner writes one and the v1 native UI reads the other,
      so the window opens on "New Session" with a blank history.
- [ ] Open the native OpenCode window from any origin the SPA is served at. It loads only
      when the tracon tab is at exactly `http://127.0.0.1:7420`; `localhost`, a LAN address
      or the hub's origin get a refused frame.
- [ ] Let the model catalogue refresh. `models.opencode.ai` is not on the gateway's
      `allow_hosts`, so the harness's catalogue fetch is refused and its model list never
      updates; fetch it node-side and serve it, or allow the one host.

## Later — earned by the loop before it

Intended changes, including a future revision of the diff-first design principle. They
wait until **Now** and **Next** have made a day's work unremarkable.

**Product engineering, the part that survived**

- [ ] Show the preview link and the proof document beside the diff in review, with the
      criteria coverage the brief already carries. The diff and runtime checks stay the
      authoritative record; this adds the two things a product judgement needs and no more.
- [ ] Separate authorization to publish, technical verification and human acceptance of the
      outcome. Support optional published/awaiting-evaluation work rather than closing it
      merely because a PR opened. Bind acceptance to the evaluated candidate and criteria;
      a new candidate must not inherit an old verdict. Plain coding tasks may still end at
      publication when that is the operator's chosen endpoint.
- [ ] Individually addressable review feedback: general and file comments and line/range
      threads bound to revision, path and side, returned through the agent review contract
      and kept across resubmission. Addressed, unresolved and outdated are distinct; a moved
      anchor must not silently attach to unrelated code. Anchoring to criteria and
      screenshots only if the proof document makes it worth it.
- [ ] Exit proof: one independent operator's complete loop on a real project — capture a
      user's problem and source, agree on good, delegate with selected context, capture a
      candidate, obtain a preview, try the task, record the result, inspect a revision and
      explicitly accept. In a PR-first preview workflow, publish the candidate, fail the
      user task in the preview, and show that the outcome stays unaccepted with an obvious
      continuation path. Fixtures are not substitutes.

**Portable data**

- [ ] Publish a versioned data contract: the signed candidate JSON, offline package reader
      and document export extended into a round-trip export readable without tracon —
      sessions, events, work items, decisions, drafts, documents, memories, workspace
      state, evidence, provenance, briefs and criteria, with identifiers, encodings and
      omission rules defined. Export omits credentials and private keys and warns that
      transcripts can still carry sensitive information; neither import nor signature
      verification grants authority. Import is proven on a fresh node and by reading with
      nothing running.
- [ ] Save the full-text conversation before any compaction or context reset, and the
      session's scratchpad, in the same package as the harness state backup, so a
      compacted context summarises something that still exists.

**Diff reading.** Borrow interaction patterns from
[cosmicspork/review](https://github.com/cosmicspork/review), especially its
[diff viewer](https://github.com/cosmicspork/review/blob/main/src/diff-part.ts); weigh
`diff2html`/`highlight.js` against the existing CodeMirror dependency first. Keep
tracon's immutable candidates, isolated checks and brokered publication.

- [ ] Side-by-side and unified modes, defaulting by width, with aligned lines, synchronized
      scrolling and a remembered preference; switching preserves file, position and
      feedback.
- [ ] Readable code and context: line numbers both sides, syntax highlighting, within-line
      emphasis, wrap controls, context expanded from the pinned base and candidate rather
      than the live worktree, and explicit labels for renames, binaries and missing context.
- [ ] File navigation and progress: collapsible sections, filtering, per-file counts,
      next/previous hunk, and viewed-file tracking against the reviewed revision that
      invalidates on resubmission. Viewed is not approved.
- [ ] Keep large reviews responsive: headers first, incremental bodies, lazy highlighting,
      generated and oversized files collapsed with a reason and a Load diff action.
- [ ] Present the outgoing title and body as sanitized Markdown with source editing and a
      preview of the exact text to be published.

## Live proofs still the operator's

Everything here is built and covered by test; what is missing is a real credential, a
real device, a real cluster or a published release. Nothing on this list is a blocker for
work that does not need it, and nothing on it may be described elsewhere as proven.

**For the supported configuration**

- [ ] **A hosted Anthropic API key, or the Anthropic subscription, end to end through the
      gateway** from a node-started Claude Code session. The subscription flow's exchange,
      refresh and an inference call were confirmed by hand against the live service on
      2026-09-15; the node doing it is not yet proven.
- [ ] **A private repository end to end**, through preparation, agent work, checks in the
      project's toolchain image, and authorized publication to GitHub, with nothing in the
      published result naming tracon.
- [ ] **The normal workflow, proven as a workflow**: client disconnect and reconnect,
      interrupted execution, retained drafts, a node restart, and recovery through
      completion, with what actually ran, what stayed uncertain, and where the operator
      intervened recorded. Fixture screenshots and fake-provider tests are not evidence.
- [ ] **The macOS desktop leg**: the bundle, its self-update against a published release,
      and the Edit menu that makes copy and paste work.

**For the experimental surfaces**

- [ ] **An OpenCode session over the v1 routes, live**: a node-started session that
      calls one of the node's MCP tools, is asked before it, and ends its turn on
      `session.idle`, against a real provider; and the native window showing that
      session's history, which the v1 store now holds.
- [ ] **Codex subscription** signed in through the node's own OAuth flow, by local callback
      and by device code, and with it whether the ChatGPT backend accepts a Codex request
      whose system prompt stays in the message array.
- [ ] **`OPENCODE_SERVER_PASSWORD` refused live**: an unauthenticated probe of a running
      session server, rather than of the fake.
- [ ] **The model-dependent adversarial cases** on OpenCode: a second identical tool call
      asking again after a gateway-mediated `once`; a subagent child session the harness
      raises for itself; and killing the server *after* a tool call was recorded.
- [ ] **Usage reconciliation against the pinned OpenCode binary and a live provider**, not
      the fake server and a stub upstream.
- [ ] **A real restart against a surviving `opencode serve`**, closing the missed turn from
      the durable stream; today the container dies with the session and the path is proven
      by test.
- [ ] **The desktop window against the real OpenCode UI origin**, wrapper and origin and PTY
      together, rather than each against a stand-in; and **the browser and PWA runs against
      the real pinned bundle**, which needs `containers/opencode-ui/build.sh` to have been
      run on the machine.
- [ ] **The UI bundle fetched from a published release**: `tracon setup` fetches and
      verifies `opencode-ui-v1.18.30.tar.gz`, but no release has published that asset yet,
      so only the offline `--ui-bundle` path has been exercised against real bytes.
- [ ] **Physical devices**: install from the always-on node over HTTPS on iOS Safari and
      Android Chrome, and confirm the framed native view authenticates with cross-site
      tracking prevention on. Safari is the open one; if the frame is refused there the
      candidate fix is `document.requestStorageAccess` from inside the frame before
      `POST /boot`, which needs a gesture and therefore a visible control.
- [ ] **The relayed run through the real hub**: `tracon session open` a session owned by
      another node, with `RUST_LOG=tracon::mesh::stream=debug` on both, confirming the
      relay's `GET /v0/streams` connection holds and `GET /v0/info` reports the same
      contract everywhere.
- [ ] **Real project workflows on Kubernetes**: preparation, a coding task, checks, review.
      An OpenCode session started on the homelab cluster and ran gated tools on
      2026-09-29 (after #291, #294); the whole workflow has not.
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

**Upstream contributions worth a bounded PR** (not blockers): a flag that turns the
web-UI fallback into a 404; a flag check on nested instruction attachment; invoking the
declared `permission.ask` hook; an LSP status event; and, if the spike confirms it, MCP
servers reaching the v2 session runner.

## Current limitations

- Everything under "Live proofs still the operator's" is unproven live, including the
  private-repository run and QA verification against a real target.
- OpenCode is driven over its v1 session routes, which offer the model the node's MCP
  tools where the v2 runner offered none (findings 22, 23); a real session has not yet
  run that way. Claude Code is the working managed harness until one has.
- Required checks run in the image, and after the preparation, the operator named for the
  repository (`[[repo]]`), and in the harness image — which has no project toolchain —
  when they named none. A session does not: it runs in the harness image and cannot fetch
  a dependency, so it learns what its checks say only by submitting. The toolchain image
  is built and pinned by hand, and preparation reruns before every check.
- Preparation's egress is the Podman backend's scoped gateway. Kubernetes has none, so a
  `[[repo]]` entry that names `egress` cannot prepare there. The gateway filters by host
  only, one holder at a time: a preparation waits behind a QA browser run, and while
  either holds it open its hosts are reachable from the whole internal network.
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
- Mesh transfers are capped at 2 MB, and an imported transfer is the carried tree under a
  fresh `git init` with no history, so a candidate moved between nodes cannot be submitted
  or published from the destination. Handoff is a portable file carried by hand today.
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

Kept as intent, off the plan until the supported configuration has earned them.

- **Mesh and cross-node work.** Credential handoff convergence and re-offer to a holder
  that missed it; mesh-wide configuration sync; cross-node session handoff (checkpoint,
  transfer with Git history fetched from the forge, lineage, fencing, chunked transfers
  past 2 MB, compatibility re-evaluated at the destination); the hub roll-ups. The
  homelab node keeps what exists running.
- **Repository environments shared across the mesh.** A `[[repo]]` entry is one node's
  configuration today, so every node that works on a repository repeats it. Share the
  recipe through the channel — the Dockerfile reference, the checks, the preparation and
  its egress presets — and never the image digest: a locally built image has a different
  one on every node, so each node builds and pins its own. Waits on the node building the
  image itself (Now) and on mesh-wide configuration sync.
- **Kubernetes parity.** A scoped egress gateway, for QA browsers and for preparation, and
  a refuse-not-drop egress policy.
- **Post-publication follow-through and product metrics.** Recording merge, deployment,
  evaluation and customer observation separately; judging tracon by interruptions, time
  to verified work and tokens per accepted change. Reconsider once outcomes exist to
  count.
- **Installation-to-first-task for another operator.** After the author's own loop is
  proven.
- **Reusable work and bounded automation.** Agent-proposed skills, profiles and recipes
  activated as manifest revisions; resource-scoped operations for custom reports; saved
  recipes for recurring procedures; batches with dependencies and one completion report;
  bounded scheduled chores through the existing supervisor; a labelled demonstration mode.
  Sequence when revisited: reliability, then profiles and effective authority, then one
  proven customization, then triggers. No workflow language, no plugin runtime.
- **Retrieval reranker:** when existing retrieval proves insufficient.
- **Client terminal:** superseded by the OpenCode PTY capability; no separate tracon
  terminal.
- **`cr-sqlite`:** only if real multi-writer convergence requires it.
- **Stacked MR automation:** decide whether stacks are preferable to feature flags first.
- **General dashboard/plugin system:** only after isolated reports and views prove
  insufficient.

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
