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

Everything found on the live runs through 2026-10-08 and in the 0.29.0 screen audit has
landed (#419–#434). The audit's states stay in `spa/scripts/ui-audit.mjs`; `--state <name>`
reproduces one. The 2026-10-10 validation of `main` on the node (#439–#446) left these:

- [ ] **A second node must not take a running node's sockets.** A scratch node started
      with its own state and config directories but the default `[gateway] harness_listen`
      replaced `/run/user/1000/tracon/harness.sock` and `egress.sock` before failing on a
      busy port. The running node kept listening on the unlinked inodes, so the gateway
      could no longer reach it: no session MCP calls, no egress decisions, until a restart.
      Before unlinking a socket path, try connecting to it, and refuse to start when
      something answers.
- [ ] **Model requests fail on a corrupted TLS record.** Transient 502s from the gateway now
      log their cause (#441): `connection error: received fatal alert: BadRecordMac` on the
      connection to `api.anthropic.com`. Two to three per session, each recovered on the
      harness's first retry. Find whether it is a reused pooled connection or the path,
      and retry it on a fresh connection in the gateway before the harness sees a 502.

## Next — the working loop, made comfortable

Needed for daily use, but not blocking it today.

**Sessions and accounting**

- [ ] While a tool call runs, its transcript line shows the tail of its output. The frame
      carries it (`tool_update.output`, #436), but neither managed harness streams a
      running call's output yet.
- [ ] **`just check` passes on the operator's own machine.** Three OpenCode integration
      tests (`opencode_adversarial` ×2, `opencode_pty::a_real_shell_is_spawned_and_read_back_through_the_proxy`)
      fail on unmodified `main` on the desktop host with "Unexpected server error" from the
      `opencode` on its PATH (Homebrew, 1.18.30), so the recipe stops before the doc tests
      and the SPA. Find what differs from CI, and have the tests use the pinned binary or
      say why they skip.
- [ ] **Say when a newer policy bundle ships than the one installed.** The 2026-10-10
      node ran a build carrying working agreements v18 under signed policy v17, and
      nothing said so: the operator learned it when `run_wait`, an unattended read in v18,
      asked for permission. Settings › Permissions & policies, and Home's readiness line,
      should name the shipped version beside the installed one and offer the preview and
      apply that already exist.

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
- [ ] **A real `origin` through the gateway.** The workspace gets a remote named `origin`
      pointing at the gateway (`http://<gateway>/git/<channel>/<owner>/<name>.git`), which
      speaks git's smart HTTP to the forge with the channel's brokered token; the token
      never enters the workspace. Reads pass (`git-upload-pack`: fetch, pull, a rebase
      onto a `main` that moved since launch), each recorded on the session with the refs
      and shas fetched. Writes are refused at the protocol (`git-receive-pack` answers
      with a pointer to `submit_review`), so publication stays review-only however git is
      invoked; a policy rule on the shell command would be text matching, and noise on
      every fetch. Scoped to the session's own repository: the gateway forwards that path
      only, so the token cannot read the operator's other private repositories. Shares its
      mechanism with lending application credentials below. LFS and submodules from other
      repositories are out until a task needs them.
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

- [ ] **Checks start warm.** A required check still prepares from the repository's base
      cache, so tracon's `just check` compiles the workspace twice from nothing (clippy, then
      nextest) and runs past five minutes on this machine. The session cache (#390) is not
      the answer: no check may build on what an agent wrote. A node-built cache of the
      default branch, made when the repository image is built and mounted read-only, is.


**Node data**

- [ ] **`doc_search` misses a document by its own words.** On 2026-10-10 a search for
      `dogfood`, from a session and from an external harness, did not return
      `note-tracon-dogfood`, nor did its exact title "Tracon dogfood failures"; a session got
      `repo-hounddogreading` instead. A longer query that day did find it. Check what the
      index holds for that document (title, slug, first chunk), and make an exact word in
      the title or slug rank it first.
- [ ] **The model list follows what the operator saves.** Saving a provider's declared
      models (Settings › Connections › Providers) writes `node.toml` and nothing else; the
      pickers change only after `probe_models_into_store` runs again, on a passing
      boundary check or the "Refresh models" button. Re-probe after saving models and
      after connecting or removing a provider, and drop the button. Keep a retry only
      where the last probe failed, saying why.

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
- [ ] Tell an OpenCode agent why the node's policy refused its call. Claude Code now reads
      "the node's policy refused this (rule …): <reason>" (#441); OpenCode's v1 permission
      reply takes only `response`, so the same refusal reaches it as a bare `reject`.
      Carry the reason another way (a tool result, or a message on the session) and keep
      it distinct from the operator declining.

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

**Screens, the rest of the 0.29.0 audit**

- [ ] About sixty polish items and nits, reproducible from the audit's states: the
      Settings layouts (the exhaustion policy row, meters, the phone section nav, clipped
      identities), the Documents screens (browser-default inputs on import and transfer,
      grey errors, duplicated actions), text formatting (missing spaces after conditional
      fragments, raw Markdown and enum values from node strings, units and plurals), the
      work item's layout, and the mirrored-review and squash editors.
- [ ] Home's composer line reads `personal · tracon ·adjust` on a phone: its separators
      come from CSS, not the Svelte trim #439 fixed, and the last one loses its space.

**The boundary, extended when a task needs it**

- [ ] More catalogue services, with an optional persistent volume per repository (a
      database's data, a browser profile).
- [ ] A catalogue entry can name an `entrypoint`. Without one, an upstream image's own
      wrapper runs: chromedp's headless-shell puts socat on `0.0.0.0:9222`, which is why the
      browser is tracon's own image (#405).
- [ ] Say when a newer `tracon-browser` digest is published than the one a `[[service]]`
      entry pins, and pin it on the operator's word (Settings, or `tracon service pin
      browser`). The entry stays a digest; only the step of copying one in goes away.
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
- [ ] **The 0.29.0 live-session fixes, live** (#428, #429): a resubmitted, published
      review updating the pull request it opened, with and without `change`; and an update
      whose revision merged `main` clearing GitHub's conflict. Each is proven against a
      fake forge or by test only. (#425 is proven: on 2026-10-10 a node-started Claude
      Code session's `ask_operator`, left unanswered for 178 s, was answered and read back
      on its third `question_status` call.)
- [ ] **The 0.29.0 follow-ups, live** (#432, #433): a provider key saved from its
      Connections card and used by a session; and a failed required check under the Podman
      runner whose refusal names the failing test. The five-minute MCP cutoff (#434) no
      longer bites: no node tool holds a call past 45 s, and on 2026-10-10 a session on
      Claude Code 2.1.295 waited out a 5 m 35 s `just check` through `review_status`
      (#417) and published #444.
- [ ] **The #436 additions, live**: a setup proposal whose run notes appear in the next
      session's operator notes, and a review under `publish.commits = keep` with an edited
      message pushed to a real forge. (The built-in skill is proven: a 2.1.295 session in
      the repository's image listed it as `tracon:repo-setup` on 2026-10-10.)
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
  that names a hand-pinned `image` has none, so its runs prepare from empty. A check run
  keeps no build output: every run of a compiled project's checks builds from nothing
  (three minutes for tracon's own test build, 2026-10-01). Sessions keep theirs, but only
  Cargo's is moved out of the tree; a bundler's cache inside `node_modules` lives and goes
  with the workspace.
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
