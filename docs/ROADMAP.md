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

Every item here was found on the 2026-09-20, 2026-09-28, 2026-09-30 or 2026-10-03 live
runs, or in the daily desktop use since.

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
      and rewriting it is an operator-approved step, never silent. Still true on
      2026-10-03: #333, published from session `01a10447`, carries a
      `tracon@localhost` commit.
- [ ] **Close the loop on a published session.** Once its review is approved and
      published, session `01a10447` (2026-10-03, #333) went on sitting `running` in its
      container, holding its egress grant, with nothing on the session naming the pull
      request: the URL is only on the review, and the agent had ended its turn rather
      than wait on `review_status`. Record the publication on the session (`published`,
      with the URL) and show it there, then suspend a published session after an idle
      period — container stopped, grant revoked, workspace kept — so a CI failure or a
      review comment can resume it. Ending it stays the operator's.
- [ ] **Open external links through a clean Linux host launcher**: the AppImage's bundled
      `xdg-open` skips KDE 6 and its library path breaks a Flatpak browser. Restore the
      host environment for the child only and keep the URL and origin restrictions.
- [ ] **Keep the machine awake while a session works.** The desktop host idle-suspended
      for 54 minutes in the middle of execute session `01a0f4b3`'s turn (2026-09-30,
      during the harness's context compaction) and the session simply stopped. Hold a
      sleep inhibitor while any turn is running or a permission is waiting (logind's
      `Inhibit` on Linux, a power assertion on macOS), release it when the node is
      idle, show in the interface when it is held, and record a suspend that happens
      anyway as an interruption rather than a silent gap. It happened again on
      2026-10-03 (session `01a10447`): about 19 minutes asleep while a permission card
      waited, and because a card's expiry is measured on the wall clock it was answered
      "denied: unanswered" the moment the host woke, leaving the agent's work in a
      `git stash`. A card's deadline should count only time the node was awake.

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
      that path; do not require forge credentials or a product brief for an
      investigation.
- [ ] Preview preparation and explain incompatibility before launch, without turning
      unsupported scripts or devcontainer features into silent host execution.
- [ ] **Service sidecars, the browser first.** A `[[service]]` catalogue in `node.toml` —
      a digest-pinned image, its port, a readiness probe — from which a session asks for a
      service by name (`service_start`, which blocks until the probe passes or a bounded
      timeout, then `service_status`), never naming an image or a command. Each service is
      its own authority under the policy bundle, so a browser can be allowed while a
      database is asked. A sidecar shares the session's network namespace, so it reaches
      exactly what the session's grants open and nothing of its own, and it stops and
      suspends with the session. The first entry is a headless browser exposing CDP on the
      session's loopback, which any harness can drive (Playwright `connectOverCDP`, a
      browser MCP, a small screenshot helper in the harness layer) without a browser in
      every repository's image or a node-run scenario language; the screenshots it takes
      are what `show_work` displays. Local databases, caches and mail catchers
      follow as entries, which is also how an application that needs services runs in a
      session without tracon reading its compose files.
- [ ] **Keep a session's build output.** Every run builds a compiled project from nothing —
      `just check` took 4m47s in session `01a10447` (2026-10-03), most of it compiling.
      Give sessions a persistent per-repository build cache (Cargo's `target/`, a bundler's
      cache, the dependencies a session added) that survives the session. Required checks
      keep starting from the trusted base cache: a directory the agent wrote must not be
      able to make the review gate pass. A node-built cache of the default branch for
      checks is the follow-on.
- [ ] **Help set up a repository once.** Node tools that draft a `[[repo]]` entry (image,
      checks, egress) from what the repository already holds — its devcontainer,
      `package.json` scripts, a `just` recipe — try it in a fresh container and report
      what failed, and propose it on a card; the node writes the entry once the operator
      approves, never the agent. Egress asks approved while trying become suggested
      `egress` entries. A built-in setup skill ships through channel manifests for managed
      harnesses, and the tool descriptions carry enough for an external one. How to run an
      application and give it data stays the agent's job on each task, with optional
      free-text notes in the operator notes, so a recipe is set up once rather than kept
      in step with the code. The agent proposes checks and setup; it cannot waive checks
      or authorize its own environment.

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
- [ ] Refusals the agent can read: what the boundary refuses and which brokered tool to
      use instead, stated in the orientation. A refused egress already asks the operator
      (`request_egress`); this covers the rest.
- [ ] **Lend application credentials through the gateway**, as model credentials are lent
      today. A broker entry names an upstream, how its key is attached, and the channel or
      repository it is bound to; the session gets the upstream as a base URL on the gateway
      and its own session token as the key (`STRIPE_BASE_URL`, `STRIPE_SECRET`), as
      environment, never a file in the workspace. Two attachment modes: a header, and
      SigV4 re-signing, which keeps an AWS SDK pointed at `AWS_ENDPOINT_URL` working
      without the secret entering the boundary. Scope is the upstream host, the binding and
      the session's lifetime, with every request (method, path, status) recorded; a
      (method, path) list is optional. What a key can do is limited where it is issued —
      test-mode, restricted or sandbox keys — which is the operator's choice of what to put
      in the broker. What cannot be lent this way can be passed as a plain secret,
      labelled as visible to the agent and to the model provider, with its upstream's
      egress opened beside it. Preparation and check runs get the same injection, which
      replaces the separate "secrets for project checks" item.

**Review and publication**

- [ ] Read a mirrored review from its owner. A review is mirrored to every node on its
      channel and its verdict is forwarded to the owner, but its worktree, candidate,
      checks, criteria and forge intent stay there, so a phone served by another node (the
      homelab node, reviewing #338 and #339 on 2026-10-04) read none of them and called the
      change stale. The node now says the review is held elsewhere and leaves approval to
      the owner's own staleness check; the phone still decides without the checks, the
      evidence or what approval sends to the forge. Fetch that detail from the owner over
      the mesh, as candidate evidence already is, bounded and refused for a third node, and
      say plainly when the owner is unreachable rather than showing an empty review.
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
- [ ] Follow a pipeline the agent started. On 2026-10-06 an external session ran a
      staging pipeline on a work repository's default branch after its merge request
      merged (`pipeline_run`, approved), then had no brokered way to see it through:
      `pipeline_status` is a snapshot, so the choices were calling it in a loop or
      polling with a host `glab` outside the broker's log. Add `pipeline_wait`, a read
      that holds up to 45 s like `review_status` and returns as soon as a job or the
      pipeline changes state, with the job list, so a failed job leads straight to
      `job_trace`. Let `pipeline_run`, `job_play` and `deploy` subscribe the session to
      the pipeline they start: record each job result as an event on the session and its
      work item, and notify through the push channel when the pipeline finishes, fails
      or stops at a manual job. Share the polling with following a merge request, so a
      pipeline outlives the request that triggered it. Following never retries, cancels
      or plays a job on its own; GitHub runs follow once the item above lands.
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

- [ ] Show the criteria coverage the brief already carries beside the diff and the shown
      work in review. The diff and runtime checks stay the authoritative record; this adds
      what a product judgement needs and no more.
- [ ] Design planning and review in the same frame: an agent-built HTML bundle proposing
      layouts or flows, attached like shown work and labelled as a proposal rather than an
      account of finished work.
- [ ] Separate authorization to publish, technical verification and human acceptance of the
      outcome. Support optional published/awaiting-evaluation work rather than closing it
      merely because a PR opened. Bind acceptance to the evaluated candidate and criteria;
      a new candidate must not inherit an old verdict. Plain coding tasks may still end at
      publication when that is the operator's chosen endpoint.
- [ ] Individually addressable review feedback: general and file comments and line/range
      threads bound to revision, path and side, returned through the agent review contract
      and kept across resubmission. Addressed, unresolved and outdated are distinct; a moved
      anchor must not silently attach to unrelated code. Anchoring to criteria and
      screenshots only if shown work makes it worth it, and screen recordings or browser
      traces beside screenshots on the same evidence.
- [ ] Exit proof: one independent operator's complete loop on a real project — capture a
      user's problem and source, agree on good, delegate with selected context, capture a
      candidate, judge it from the diff and the shown work, record the result, inspect a
      revision and explicitly accept. Show that a candidate whose shown work fails the
      user task stays unaccepted with an obvious continuation path. Fixtures are not
      substitutes.

**The boundary, extended when a task needs it**

- [ ] More catalogue services, with an optional persistent volume per repository (a
      database's data, a browser profile).
- [ ] A keyed per-repository stash for artefacts that are expensive to rebuild
      (`stash_put` / `stash_get`, keyed on a digest of named input paths so a stale one is
      never silently reused); cached "before" screenshots of the default branch are a
      candidate. Knowledge stays in memories; this holds bytes. A seeded database is cheap
      to rebuild and does not justify it.
- [ ] A brokered provider CLI for operations an agent is asked to do (list a bucket,
      invalidate a cache): an operator-configured binary run on the node with the
      credential as environment only, never in argv, output bounded and redacted.
- [ ] A node-side TCP forward for a remote service that is not HTTP, with its credential
      passed as a visible secret. A local catalogue service is the better answer for
      development and comes first.

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
      and the Edit menu that makes copy and paste work. And, behind a Podman machine: an
      image built from a repository's Dockerfile, and the per-client egress proxy reached
      over `[gateway] egress_port` rather than a socket.

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

**Upstream contributions worth a bounded PR** (not blockers): a flag that turns the
web-UI fallback into a 404; a flag check on nested instruction attachment; invoking the
declared `permission.ask` hook; an LSP status event; and, if the spike confirms it, MCP
servers reaching the v2 session runner.

## Current limitations

- Everything under "Live proofs still the operator's" is unproven live, including the
  private-repository run.
- OpenCode is driven over its v1 session routes, which offer the model the node's MCP
  tools where the v2 runner offered none (findings 22, 23); a real session has not yet
  run that way. Claude Code is the working managed harness until one has.
- Required checks run in the image, and after the preparation, the operator named for the
  repository (`[[repo]]`), and in the harness image — which has no project toolchain —
  when they named none. A Claude Code session on Podman runs in that same image with the
  harness layered on, so it has its checks' tools, and reaches the registries its
  repository opens to sessions (`session_egress`). An OpenCode session, and any session
  on Kubernetes, stays in the harness image and reaches no registry. An image the node
  builds from a repository's Dockerfile is built on Podman only, and a superseded one is
  removed only when nothing still runs from it.
- A run's dependency cache and each check's tree are copies. Where the runtime's storage
  has no reflinks they are full copies, made once per run and once per check. The base
  cache they start from is filled when the node builds a repository's image; an entry
  that names a hand-pinned `image` has none, so its runs prepare from empty. A run keeps
  no build output: every run of a compiled project's checks builds from nothing (three
  minutes for tracon's own test build, 2026-10-01).
- Per-client egress — a session's, a preparation's — is the Podman
  backend's. The Kubernetes backend issues no grants, so a `[[repo]]` entry that names
  `egress` cannot prepare there. A grant filters by host, not by method: a registry that
  accepts uploads accepts them from the client that was granted it.
- macOS releases are unsigned — the publisher holds no Apple Developer ID — and are
  authenticated by GitHub build provenance instead, so Gatekeeper asks once on first open
  (right-click Open, or System Settings > Privacy & Security > Open Anyway).
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
- Nothing reaches into the boundary. A provider that delivers webhooks through an
  outbound forwarder (`stripe listen`) works with egress and a credential, and an OAuth
  callback lands wherever the browser that followed it runs; a provider that must reach a
  public URL has no route in.
- What an agent shows is its own account, not verification: the node vouches for the
  revision and the required checks, not for a screenshot.

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
  one on every node, so each node builds and pins its own. Waits on mesh-wide
  configuration sync; a node already builds the image itself.
- **Kubernetes parity.** Per-client egress grants, for sessions and for preparation —
  the node already serves the proxy there; it needs the grants — a refuse-not-drop
  egress policy, the egress ask, and service sidecars as pods of their own. What earns it:
  work left running unattended belongs on an always-on node, which is the homelab
  cluster, not a laptop that sleeps. Deferred until the laptop node is good.
- **Post-publication follow-through and product metrics.** Recording merge, deployment,
  evaluation and customer observation separately; judging tracon by interruptions, time
  to verified work and tokens per accepted change. Checking a deployed preview
  environment after its pull request opens belongs here too, without requiring the
  application to report its build commit to tracon. Reconsider once outcomes exist to
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
- Required tracon configuration files installed into project repositories, or any other
  requirement on a repository made for tracon's sake: an endpoint or header, a seeder, a
  tool in its image.
- Publishing shown work to the forge.
- A declarative browser scenario language run by the node; the agent drives the browser.
- Intercepting TLS to attach credentials; a credential is lent only where the client
  points at the gateway.
- A full IDE, general file editor, or per-project editor configuration.
- Business-domain features such as invoicing and billing.
- Arbitrary host execution through the node API. Service/CLI installation and node restarts
  remain explicit desktop/CLI operations; file imports use a picker/upload flow.
