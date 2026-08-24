# Roadmap

Phases are ordered so each one is independently useful. Nothing is retired until its
replacement has been in real use.

Scope discipline matters more here than anywhere else in the design. This is six
products (control plane, runner fleet, credential broker, memory service, work ledger,
clients) and the failure mode is building all six halfway.

The phases are ordered by unique value, not by convenience. Browser-driven chat over an
agent already exists (opencode web, Claude Code web) and has been used enough to know it
is preferred to a TUI. That question is settled and does not need a phase. What nothing
else provides is enforcement, then cross-machine reach; those come first.

## Phase 0: Validate the assumptions

No code. Everything below is load-bearing for the design and cheap to check. Results
and the exact commands for the remaining work-side checks are in `VALIDATION.md`.

- [x] **Read a real ACP session end to end.** ACP framing is one JSON object per line, so
      tee both directions through named pipes into log files and read an `omp acp`
      session before writing any Rust.
- [x] **Drive one real task with the harness under restriction.** Network closed, no
      `gh` / `glab` / `acli` on PATH, no `.env`. The gate assumes the harness can be told
      "these actions go through a tool" and will cooperate rather than fight. Find out
      whether omp / opencode / Claude Code degrade gracefully or thrash before the
      broker is built around that assumption.
- [ ] **Check the devcontainer Docker socket situation.** If `devcontainer.json` enables
      the docker-in-docker or docker-outside-of-docker feature, or mounts
      `/var/run/docker.sock`, the harness has the daemon API and the gate cannot be
      enforced. It has to be removable; there is no advisory mode to fall back to.
- [ ] **Find the Coder autostop policy.** If workspaces stop aggressively, background
      work at work is off the table regardless of what gets built.
- [ ] **Confirm the inner devcontainer is not privileged** and has no excessive caps.
- [x] **Stand up the personal boundary by hand.** Rootless podman on Bazzite, harness in
      a container on an internal network, node-side process holding the exec pipe.
      Confirm the harness can still reach the model provider through the node and
      nothing else.
- [ ] **Confirm Python and `uv` are available in the outer Coder pod**, or can be
      fetched by the bootstrap. The consulta sidecar runs there, not in the
      devcontainer.
- [x] **Confirm the DigitalOcean embeddings and reranking models** available on
      serverless inference, and price a batch inference job against synchronous for
      nightly backfill.
- [x] **Decide the human-time signal.** Claim/release on approval items. Browser
      heartbeat measures tab focus, not attention, and puts the SPA in the loop for a
      metric. Decided here because it changes the event schema and is expensive to
      retrofit.

Exit criteria: the privilege boundary is known to be achievable on both the work pod
and the personal machine, the harness is known to tolerate restriction, and the ACP
message shapes are understood well enough to write the adapter.

## Phase 1: Node and gate, one machine, no mesh

The smallest thing that changes behavior. Personal machine only; the work node comes
in Phase 3. Built directly on the host with Claude Code or omp; that path is kept
forever as the bootstrap escape hatch. Dogfooding starts once this phase exits.

Node:

- [ ] Rust binary, static musl, `x86_64` and `aarch64`
- [ ] Personal boundary as code: internal network, gateway container with the
      allowlist proxy and node-socket forward, harness container. Exactly what
      `VALIDATION.md` proved by hand.
- [ ] Startup boundary check; refusal to run harnesses surfaced with the failed check
- [ ] ACP adapter for omp, with a harness adapter trait from day one. One harness until
      a concrete task needs a second; adapters are the part that rots.
- [ ] Session lifecycle: worktree creation and branch, git identity materialized,
      spawn inside the boundary, stream, teardown
- [ ] Config materialized to a scratch directory, nothing written to the repo
- [ ] Embedded SPA (`rust-embed`), served locally: queue, session screen with streaming
      output and prompt input, approval detail, nodes, new-session form with a required
      model field
- [ ] Local SQLite: `node`, `session`, `event` tables
- [ ] `work_item_id` nullable column on `session` and `event` from the start, even
      though the ledger does not exist yet. Adding the graph later is additive; not
      having the column forces a rewrite.
- [ ] Durations recorded monotonically on the node that observed them

Gate:

- [ ] Permission handling: ACP `session/request_permission` routed to the queue
      (Claude Code `control_request` when that adapter lands)
- [ ] Local policy evaluation, signed bundles, fail closed on approve
- [ ] Credential broker, sealed, harness has no read path
- [ ] **consulta absorbed as the first brokered tool.** Node exposes `query` and
      `describe` MCP tools; the Python sidecar is spawned and owned by the node with the
      DB credential injected from the broker. Guard ported to Rust (`sqlparser-rs`) so the
      node refuses before spawning; consulta's own guard stays as the second, independent
      check. Smallest blast radius of any credential, read-only by construction, and it
      exercises the whole tool → node → broker → external path before `gh` does.
- [ ] `gh` behind the broker; the harness gains a push path only through it
- [ ] Absorb the `review` submit schema and verdict contract
- [ ] `/revise` flow, including code edits (agent stays the only worktree writer)
- [ ] Blob hash recorded at submit, stale-diff conflicts surfaced
- [ ] Tool surface reduction before gating (`--tools`, `--disallowedTools`)
- [ ] Encode the five working agreements as policy: worktree not main checkout, review
      before publish, no merge, no transition, no production deploy

Exit criteria: a full task is driven from the browser on the personal machine, start to
finish, with `gh` and the consulta credential reachable only through the node. From
here on, tracon is built through tracon.

## Phase 2: Mesh

- [ ] Hub deployed to the homelab cluster **as the relay**: opaque frame routing, keyed
      per channel, modeled on pager and svastha. Sync and processing come in Phase 4;
      here it only routes.
- [ ] Node keypair on first run, enrollment via short-lived code from an enrolled node
- [ ] Channel model and key scoping
- [ ] Policy signing key generated and kept off the hub
- [ ] Second node in a Coder workspace, bootstrapped by one-line binary fetch, not
      dotfiles
- [ ] Harness state directory as a node-owned volume (the `OMP_STATE_DIR` pattern)
- [ ] SPA connects to the mesh regardless of which node served it

Exit criteria: a session running in a Coder workspace is visible and controllable from
the browser on the personal machine, and the latency complaint is resolved.

## Phase 3: The work node enforces

The work topology has the privilege boundary the personal machine lacks. Use it.

- [ ] Node in the outer Coder pod, harness in the inner devcontainer via `docker exec -i`
- [ ] Devcontainer on an internal Docker network with the node as the only route out
- [ ] Startup boundary check passes on the live pod
- [ ] Broker holds `glab`, `acli`; consulta bound to the work channel and the work node
- [ ] Work-channel policy: no merge, no transition, no production deploy, enforced at
      the broker rather than the prompt

Exit criteria: an agent physically cannot post to GitLab or Jira, or reach the work
database, except through the node, and `review` and `consulta` can be archived.

## Phase 4: Memory, documents, hub

- [ ] Hub-and-spoke sync: site ID, monotonic sequence, hub-assigned ordering, cursor
      pull, HLC plus LWW
- [ ] Full local replica on every node, reads always local
- [ ] Hub decrypts personal and client channels; work channels ordered and forwarded
      opaquely
- [ ] Document store, notebook prefix scheme as the kind column
- [ ] Memory store: scope and kind axes, MCP `retain` / `recall` tools
- [ ] Bank identity from channel and project ID, never cwd or git root
- [ ] **Retrieval starts FTS5-only.** Directives ranked above facts. Add `sqlite-vec`
      and a reranker only once FTS demonstrably misses things in real use; the corpus is
      plain text either way and vectors are the part that does not export.
- [ ] Embedding model name and dimension stored per vector row, when vectors arrive
- [ ] Processing node and provider bindings, enforced fail-closed, recorded as data
- [ ] Local embeddings on the work node, when vectors arrive
- [ ] Generated per-session orientation file: shared conventions, node facts, channel
      policy
- [ ] Memory promotion routed through the approval queue, batched nightly
- [ ] Degraded mode verified: hub unreachable means FTS-only, work continues
- [ ] Encrypted hub snapshots to object storage, restore exercised at least once

Import both workspace `README.md` files and the `docs/` corpus. Run
`workspace-notes-sync` in parallel until the machines demonstrably converge, then cut it.

Exit criteria: `notebook` can be archived, and the two orientation files stop drifting
because there is only one source.

## Phase 5: Ledger, phases, metrics

- [ ] Work item store: hash IDs, dependency edges, `discovered-from`
- [ ] Ready-work query, deterministic topological sort
- [ ] Session requires a work item; ready-work injected at start; session ends at item
      close
- [ ] Plan / execute / review as separate spawned sessions
- [ ] Model required in every session spec, validated at spawn
- [ ] Per-session budget enforced by killing the session
- [ ] Deterministic supervision between phases (`just check`, `just analyse`,
      `just test`)
- [ ] Review sessions run with fresh context: requirements and diff only
- [ ] Diff size cap at submit
- [ ] Execute phase gated on a plan artifact
- [ ] Metrics rollups: approvals per accepted change, cost per accepted change
- [ ] Per-channel daily cost ceiling enforced by the node
- [ ] Provenance queryable per commit: model, prompt, approval, policy version

Exit criteria: subagent model inheritance is structurally impossible, and cost per
accepted change is a number you can read.

## Phase 6: Clients

- [ ] PWA: manifest, service worker, no local replica, no keys at rest
- [ ] Notification sinks bound per channel (pager for personal and client)
- [ ] Tauri desktop wrapper: tray, command-tab, global hotkey, system notifications
- [ ] Node supervised by the platform (systemd user unit on Bazzite, launchd on macOS,
      container lifecycle in Coder); the wrapper is a tray client of it, not its
      supervisor. Switchboard's units move, its role is not reimplemented.
- [ ] Work approvals surface in the wrapper tray
- [ ] `@codemirror/merge` editable diffs feeding `/revise`
- [ ] Retire Switchboard, after supervision has moved and notebook and review are gone
- [ ] Rehome or deliberately kill the display-linked theme switcher

Exit criteria: a task can be directed from the phone with both laptops closed, and
`switchboard` can be archived.

## Deferred

Not rejected, not scheduled.

- **Second and third harness adapters** (opencode, Claude Code). The trait exists from
  Phase 1; the adapters land when a task needs them.
- **Vector retrieval.** FTS5 first; see Phase 4.
- **Terminal in the client.** Add only when something concrete cannot be done without
  it. `xterm.js` over the existing session channel, deliberately minimal.
- **`cr-sqlite`.** Only if real multi-writer convergence becomes necessary. The sync
  design keeps the changeset shape so this stays available.
- **Stacked MR automation.** Resolve the flags-vs-stacks question first. The trunk plus
  semantic-release setup favors feature flags, which also produces smaller reviews and
  helps the complexity problem.
- **Model-proxied credentials.** Routing model API calls through the broker with an
  injected key is a TLS-interception project and is not where the risk is.
- **Splitting the relay back out of the hub.** Channel-scoped keys make this possible if
  the hub's host ever becomes less trusted than somewhere else available. Not needed
  while everything always-on lives in the same cluster.

## Never

Recorded so they are not rediscovered as good ideas.

- **Multi-user.** No tenancy in the auth model. Single operator by design.
- **Business domain in tracon.** Clients, invoicing, and time billing stay in
  Kritee.
- **An agent loop in the node.** Harnesses are replaceable; a harness written here is
  not. Considered and rejected: owning the harness is a large undertaking whose main
  return is self-controlled churn.
- **Anything written into project repos.**
- **A full editor.** Diff editing only. The moment a file tree, LSP wiring, and a
  settings pane appear, this has become a second project.

## Risk register

| Risk | Mitigation |
|---|---|
| Harness protocol drift | Version pinning, recorded per session, compatibility check at session start |
| Harness fights the restriction | Phase 0 drives a task under restriction before the broker is designed around cooperation |
| Scope creep into an IDE | The "Never" list above; terminal and editor deferred by default |
| Hub becomes load-bearing | Degraded mode specified and tested, not assumed. Relay outage: auto-allow continues, approvals block |
| Hub compromise | Work channel opaque to it, policy signing key never on it, per-client keys |
| Bootstrap lockout | Documented path to running a harness outside the system, maintained |
| A node cannot establish its boundary | Phase 0 proves it by hand on both machines; the startup check refuses rather than degrades |
| Corpus lock-in | Boring schemas, plain-text export for every kind; vectors deferred |
| Cost runaway | Per-session budget and per-channel daily ceiling, enforced not monitored |
| Retiring Switchboard too early | It retires last, after supervision has moved |
