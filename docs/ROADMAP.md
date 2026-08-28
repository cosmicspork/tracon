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

Validation work completed on 2026-08-24. The ACP, restricted-harness, and Bazzite
boundary assumptions passed. The assumed work Coder boundary failed: the current
single-container template cannot enforce the gate. Evidence remains in the
[`ACP capture`](reference/acp-omp-18.0.4-session.jsonl),
[`restricted-session capture`](reference/acp-omp-restricted-session.jsonl),
[`restricted-session driver`](reference/acp-drive-restricted.py), and
[`gateway configuration`](reference/gateway-tinyproxy.conf).

- [x] **Read a real ACP session end to end.** `omp acp` 18.0.4 uses newline-delimited
      JSON-RPC. The capture establishes session configuration, tool and permission
      updates, streaming output, and usage events needed by the adapter.
- [x] **Drive one real task with the harness under restriction.** With network closed,
      no publishing CLIs, and no `.env`, omp completed local work, reported denied
      publishing accurately, and stopped without retries or attempts to route around
      the gate.
- [x] **Check the devcontainer Docker socket situation.** The current template exposes
      neither Docker CLI nor `/var/run/docker.sock`, and the sampled devcontainer files
      declare neither Docker feature nor socket mount. That does not establish a
      boundary: the template's sole Envbuilder container is privileged.
- [x] **Find the Coder autostop policy.** The configured autostop duration is 8 hours.
      Background work therefore needs an active workspace or explicit restart handling.
- [x] **Check the container privilege and capabilities.** This fails the original
      topology assumption: the template sets `privileged = true`; `coder` has
      passwordless `sudo`; root has `CapEff: 000001ffffffffff`. There is no separate
      inner runner, so this topology cannot run a gated harness.
- [x] **Stand up one host boundary by hand.** Rootless Podman on Bazzite, harness in a
      container on an internal network, node-side process holding the exec pipe.
      Confirm the harness can still reach the model provider through the node and
      nothing else.
- [x] **Check Python and `uv` in the Coder environment.** The template creates no
      separate outer pod: Coder runs in the Envbuilder devcontainer. It has Python
      3.9.2; `uv` is absent but its installer is reachable. The replacement topology
      must provision sidecar dependencies independently.
- [x] **Check DigitalOcean embeddings, reranking, and batch inference.** Embeddings use
      synchronous endpoints and are not supported by batch inference. Reranking is a
      knowledge-base feature, not a standalone endpoint. Candidate embedding models are
      deferred to Phase 4, after FTS demonstrates a need.
- [x] **Decide the human-time signal.** Claim/release on approval items. Browser
      heartbeat measures tab focus, not attention, and puts the SPA in the loop for a
      metric. Decided here because it changes the event schema and is expensive to
      retrofit.

Outcome: the adapter, restricted-harness, and Bazzite boundary checks passed. The
original work-boundary criterion failed and produced the Phase 3 replacement-topology
requirement. It did not create an advisory mode or establish the current Coder workspace
as a valid node.

## Phase 1: Node and gate, one machine, no mesh

The smallest thing that changes behavior. Phase 1 runs on any one machine that can host
the persistent node and isolated runner, retain state, expose the SPA to the operator,
and pass the startup boundary check. Bazzite proved one implementation; it is not the
required platform. macOS, another Linux host, or a managed environment qualifies by
capability, not product name.

Claude Code for web, omp, or another harness may be used outside tracon to implement
this phase. That is the bootstrap escape hatch, not a claim that the implementation
environment can run the node. Dogfooding starts once one eligible machine passes the
checks and completes the phase.

Node:

- [x] Rust binary, static musl, `x86_64` and `aarch64`. Built with `cargo zigbuild`
      and asserted static in CI on every change, so a dependency that cannot
      cross-compile is caught by the change that adds it. macOS builds natively and is
      not musl.
- [x] Host boundary as code: internal network, gateway container with the allowlist
      proxy and node-socket forward, harness container. Exactly what Phase 0 established
      by hand on Bazzite, generalized behind capability checks.
- [x] Startup boundary check; refusal to run harnesses surfaced with the failed check
- [x] ACP adapter for omp, with a harness adapter trait from day one. One harness until
      a concrete task needs a second; adapters are the part that rots.
- [x] Session lifecycle: worktree creation and branch, git identity materialized,
      spawn inside the boundary, stream, teardown
- [x] Config materialized to a scratch directory, nothing written to the repo
- [x] Embedded SPA (`rust-embed`), served locally: queue, session screen with streaming
      output and prompt input, approval detail, nodes, new-session form with a required
      model field. Built in the Ledger × Tonal direction settled 2026-08-24. The approval
      screen is a stub until the review contract lands; there is nothing to approve yet.
- [x] Local SQLite: `node`, `session`, `event` tables
- [x] `work_item_id` nullable column on `session` and `event` from the start, even
      though the ledger does not exist yet. Adding the graph later is additive; not
      having the column forces a rewrite.
- [x] Durations recorded monotonically on the node that observed them

Gate:

- [x] Permission handling: ACP `session/request_permission` routed to the queue
      (Claude Code `control_request` when that adapter lands)
- [x] Local policy evaluation, signed bundles, fail closed on approve
- [x] Credential broker, sealed, harness has no read path
- [x] **consulta absorbed as the first brokered tool.** Node exposes `query` and
      `describe` MCP tools; the Python sidecar is spawned and owned by the node with the
      DB credential injected from the broker. Guard ported to Rust (`sqlparser-rs`) so the
      node refuses before spawning; consulta's own guard stays as the second, independent
      check. Smallest blast radius of any credential, read-only by construction, and it
      exercises the whole tool → node → broker → external path before `gh` does.
      Verified against a real harness: the agent queried a database it has no credential
      for, and a `DELETE` was refused by the node before the sidecar was spawned. The
      Oracle profile is a credential-store entry away; only SQLite has been exercised.
- [x] `gh` and `glab` behind the broker; the harness gains a push path only through it
- [x] Absorb the `review` submit schema and verdict contract
- [x] `/revise` flow, including code edits (agent stays the only worktree writer).
      "Request changes" keeps one evolving thread: the notes go back through
      `review_status`, the agent edits and resubmits against the same review. Editing a
      diff in the browser and submitting it back is Phase 6, with the desktop wrapper;
      the operator asks for the change and the agent makes it, which keeps the agent the
      only writer to the worktree.
- [x] Blob hash recorded at submit, stale-diff conflicts surfaced
- [x] Tool surface reduction before gating (`--tools`), available per node but **off by
      default**. omp's `--tools` is a whitelist that does not accept its shell, so any
      list at all removes the shell, and without a shell the agent cannot commit and the
      review contract has nothing to carry. Found by running it: the agent did not report
      being stuck, it began reading `.git/refs` by hand instead. Reduce deliberately once
      you know what a channel needs; the boundary and policy bound the surface either way.
- [x] Encode the five working agreements as policy: worktree not main checkout, review
      before publish, no merge, no transition, no production deploy. Shipped as the
      starting bundle, so the rules are data from the first run rather than prose.

Session budgets are enforced in tokens, accumulated from each turn's reported usage and
checked when the turn ends. A single long turn can therefore overshoot: ACP reports usage
per turn, not continuously. Mid-turn enforcement needs a usage snapshot the adapter does
not have yet, and per-channel ceilings are Phase 5.

Phase 1 completed 2026-08-25. The node establishes and
verifies its boundary, spawns `omp` inside it, runs a session end to end (worktree, prompt,
permission request routed to the queue, answer, budget kill, teardown), streams events over
SSE, and serves the interface those screens were designed for. The broker now holds
credentials the harness cannot read, and reaches them out as MCP tools over the gateway
forward, authorised per session. Review before publish is enforced rather than instructed:
the agent has no forge token, the node captures the diff and publishes the approved bytes,
and a branch that moved after submit cannot be approved. Policy decides what the node
answers without asking, what it refuses with a reason, and what reaches the queue; the
five working agreements ship as its starting bundle.

Not yet dogfooded: the exit criterion says tracon is built through tracon from here, and
that has not happened yet — this phase was built with an external harness, which is the
bootstrap escape hatch the phase allows. The first real test of the exit criterion is
using it.

Exit criteria: a full task is driven from the browser against one boundary-capable node,
start to finish, with `gh` and the consulta credential reachable only through the node.
From here on, tracon is built through tracon.

## Phase 2: Mesh

- [x] Hub deployed to the homelab cluster **as the relay**: opaque frame routing, keyed
      per channel, modeled on pager and svastha. Sync and processing come in Phase 4;
      here it only routes. Built as the `hub/` crate (`tracon-hub`): signed requests
      with the signature as the replay nonce, per-channel sequence, cursor pull with a
      `410` resync when behind retention, payload-free SSE pokes, members as routing
      metadata, enrollment slots. It holds no channel keys and opens no frame. The
      homelab manifests are `kubernetes/apps/{base,production}/tracon-hub`; the pod
      goes live with the first release that publishes the image.
- [x] Node keypair on first run, enrollment via short-lived code from an enrolled node.
      `<state>/node-identity.seed`; the node id is the Ed25519 public key (a Phase 1
      database is rekeyed once). `tracon mesh invite` / `tracon enroll`, or the Enroll
      screen: slot on the hub, public keys and a name in the clear, fingerprints
      compared by the operator before admission, then keys and policy handed off
      direct-sealed.
- [x] Channel model and key scoping. A channel is a keyring of epochs wrapped to each
      member node; frames seal under the newest epoch with the channel and epoch bound
      into the AEAD data, so the hub cannot re-label a frame. `tracon channel create`,
      handoff by enrollment, union merge on re-handoff. Bindings are recorded per
      channel; only membership is enforced in this phase.
- [x] Policy signing key generated and kept off the hub. Unchanged where it lives;
      bundles travel as direct-sealed frames, the public key is trusted only from the
      enrollment handoff, and a bundle signed by any other key is refused and shown.
      `tracon policy push` hands a new bundle to every member; it takes effect without
      a restart.
- [x] Second boundary-capable node on another host, bootstrapped by one-line binary
      fetch, not dotfiles. `install.sh` fetches the static musl binary and verifies
      the release checksum; the container definitions ship inside the binary so
      `tracon setup` builds the images. Stood up on the Bazzite host: the Linux
      gateway forward is a Unix socket, and two SELinux constraints were found by
      running it ([`phase-2-notes`](reference/phase-2-notes.md)).
- [x] Harness state directory as a node-owned volume (the `OMP_STATE_DIR` pattern).
      The volume is the only credential store the harness sees; the operator's
      `~/.omp` is never mounted again. `tracon harness import-credentials` copies a
      store in once; `tracon harness shell` logs in where none exists. `OMP_STATE_DIR`
      is set but unverifiable against omp's stripped binary, so the mount stays at
      `/root/.omp`.
- [x] SPA connects to the mesh regardless of which node served it. The interface talks
      only to the node that served it; that node mirrors every peer's sessions,
      requests, and reviews into the same tables scoped by node, and forwards
      prompt / answer / kill / verdict / create to the owner as sealed commands. A
      prompt to an unreachable owner is queued and sent when it returns; the rest fail
      honestly. Node chips everywhere, held cards for unreachable owners, one quiet
      hub banner, an Enroll screen, and a node pick within a channel's binding.

Phase 2 implementation completed 2026-08-28. The exit criterion is demonstrated
end to end in-process (`node/tests/mesh_e2e.rs`: two managers with the fake harness, two
live mesh clients, one real hub router; a session created on B from A's API, its outcome
and events mirrored back, a forwarded kill refused as the owner phrases it, a queued
prompt to an unreachable owner). The two-machine run — hub on the homelab cluster, the
macOS node and the Bazzite node enrolled, a session on one driven from the other's
browser — waits on the first release (which publishes the hub image and the node
binaries) and the homelab merge; the Bazzite node already passes its boundary check with
the socket forward.

Not built in this phase, recorded so it is not assumed: live chunks and tool progress are
not forwarded (the remote view is message-granular; the persisted message arrives at
turn end); drafts are per interface, not mirrored; channel bindings beyond membership
are recorded but not enforced; `tracon channel rotate` (a new epoch plus re-handoff) is
a small follow-up on the keyring that already supports it.

Exit criteria: a session running on either eligible node is visible and controllable
from the browser served by the other, proving cross-machine reach without depending on
the blocked work topology.

## Phase 3: The work node enforces

The current work topology is privileged and cannot enforce the gate. Phase 3 replaces
it with a boundary the node can verify.

- [x] Replace the privileged single-container Envbuilder workspace with a node-owned,
      unprivileged harness runner topology. Built as a second boundary backend
      (`[runtime] kind = "kubernetes"`): the node is an unprivileged pod that creates
      one harness pod per session, and two NetworkPolicies make it the harness's only
      route. Proven on the homelab cluster (`deploy/kubernetes/lab`,
      [`phase-3-notes`](reference/phase-3-notes.md)); the Coder template that carries
      it to the work cluster is written on a host that can reach that environment,
      against [`deploy/coder/README.md`](../deploy/coder/README.md).
- [x] Node outside the harness container, with a node-owned exec pipe. `pods/attach`
      over a WebSocket the node opens; killing a session deletes the pod.
- [x] Harness runner on an internal network with the node as its only route out. No
      resolver, no API token; egress only to `tracon.dev/role=node` on the forward and
      proxy ports; the node serves the CONNECT allowlist proxy itself.
- [x] Startup boundary check passes on the live pod. The same five checks, answered
      from a gated probe pod, the policy's shape, and a `SelfSubjectAccessReview` per
      verb; all five pass on the lab pod. The work pod is pending the template.
- [x] Broker holds `glab`, `acli`; consulta bound to the work channel and the work
      node. As credentials (`glab`, `jira`) behind narrow REST tools — `mr_status`,
      `mr_comment`, `issue`, `issue_comment` — and a `nodes` binding on every
      credential, so "work node only" is data.
- [x] Work-channel policy: no merge, no transition, no production deploy, enforced at
      the broker rather than the prompt. Those verbs are not tools; the token that could
      do them never leaves the node. Policy also decides every tool call before the
      broker is touched, and a call the bundle does not name is asked, not run.

Phase 3 implementation completed 2026-08-28. The runner topology is built and proven
on the homelab cluster: all five checks pass inside the node pod, the deep probe shows
a harness pod with no route but the node, and a session creates its pod, mounts the
worktree as subpaths of the shared claim, and attaches. Not yet done: a turn with model
credentials in the lab, the Coder template itself, and therefore the boundary check on
the live work pod — that is the operator's next step on a host that can reach it.
`review` and `consulta` can be archived once the work node has run real tasks through
these tools.

Exit criteria: an agent physically cannot post to GitLab or Jira, or reach the work
database, except through the node, and `review` and `consulta` can be archived.

## Phase 4: Memory, documents, hub

- [x] **Model credentials brokered.** The harness holds a placeholder key (its session
      token) and reaches every provider through `/model/<provider>/…` on the node's
      harness listener; the node injects the credential from its sealed store
      (`credentials.sealed`, under a key derived from the identity seed), enforces the
      channel's provider bindings, and counts usage per request (`GET /api/usage`).
      Login and refresh run `omp auth-broker login` / `omp token --force-refresh` as
      node-owned subprocesses against a per-provider store never mounted into a
      session; the Nodes screen carries the "connect a provider" card with the
      sign-in link and the paste-back. `agent.db`, `harness import-credentials`, and
      `harness shell` are gone. The spike behind it and what could not be verified (a
      live Anthropic subscription token: the operator's had expired) are in
      [`phase-4-notes`](reference/phase-4-notes.md).
- [x] Hub-and-spoke sync: site ID, monotonic sequence, hub-assigned ordering, cursor
      pull, HLC plus LWW. The `sync` crate: `change_log` keyed on `(site, site_seq)`, a
      persisted HLC, row-level last-writer-wins on `(hlc_ms, hlc_ctr, site)`, tombstones;
      `changes` / `changes_request` / `changes_batch` payloads under contract version 2.
      Found on the way and fixed: the Phase 2 client deadlocked on its first hub outage.
- [x] Full local replica on every node, reads always local. Documents, memories, and
      batches are replicated tables on every node; recall and the interface never wait
      on the hub, and a late joiner backfills each site's log when it is handed a key.
- [x] Hub decrypts personal and client channels; work channels ordered and forwarded
      opaquely. The hub has an identity and a `hub.db` replica; `tracon channel share
      --hub` admits it into a channel (role `hub`) and hands it the keyring. A channel
      nobody shares stays ciphertext because there is no key, and the hub counts what it
      could not open.
- [x] Document store, notebook prefix scheme as the kind column. `tracon doc
      import|ls|get|put|rm|export`, `GET/PUT/DELETE /api/docs/{channel}/{slug}` with the
      notebook's `If-Match`/412 contract, `doc_read` / `doc_search` / `doc_write` for
      agents (the write is asked), and a Documents screen with search, markdown, and a
      conflict-aware editor. `~/src/docs` (12 documents) and the workspace README
      imported.
- [x] Memory store: scope and kind axes, MCP `retain` / `recall` tools. Directives are
      the operator's (`tracon memory add`, `POST /api/memories`); `retain` writes facts,
      lessons, and episodes, and holds a lesson (or a fact under 0.7 confidence) as a
      candidate for the batch. Every session gets the MCP server now.
- [x] Bank identity from channel and project ID, never cwd or git root:
      `sha256(channel ‖ canonical remote)`, recorded as `session.project_id`.
- [x] **Retrieval starts FTS5-only.** Directives ranked above facts (by confidence and a
      90-day half-life), then promoted lessons, then documents with snippets; episodes
      only when asked. `sqlite-vec` and a reranker wait for FTS to demonstrably miss.
- [ ] Embedding model name and dimension stored per vector row, when vectors arrive
- [x] Processing node and provider bindings, enforced fail-closed, recorded as data. A
      channel's `bindings_json` carries `providers` (the gateway refuses anything off the
      list) and `processing` (`hub` or a node id: who batches it). Both are handed off
      with the keyring.
- [ ] Local embeddings on the work node, when vectors arrive
- [x] Generated per-session orientation file: guides on the channel, this node's facts,
      the bundle's deny rules with reasons, then directives and confident facts —
      capped, mounted read-only, passed as `--append-system-prompt`, recorded as an
      `orientation` event. Nothing in the worktree.
- [x] Memory promotion routed through the approval queue, batched nightly. The batch is
      a synced `promotion` record built by whoever the channel's `processing` binding
      names (the hub at 03:00 UTC, the node at `[memory] promote_at`); verdicts are
      per item, local writes, and converge; the third queue kind, after reviews.
- [x] Degraded mode verified: hub unreachable means FTS-only, work continues
      (`node/tests/sync_e2e.rs`: recall and the outbox with the hub answering 503,
      concurrent offline edits converging on its return, a delete across a second
      outage).
- [x] Encrypted hub snapshots to object storage, restore exercised at least once —
      against a directory bucket in `hub/tests/snapshot.rs`; the run against Spaces
      waits on a bucket and a key in the cluster's secrets.

Imported: `~/src/docs` and this host's workspace `README.md` (as `guide-workspace`).
The work-side README is imported from a host that can reach it. There was no
`workspace-notes-sync` on this host to run in parallel; the notebook stays runnable
against `tracon doc export` until the operator has lived in the Documents screen.

Phase 4 implementation completed 2026-08-28 except the vector items, which wait on
FTS missing something. Not done, recorded so it is not assumed: a live subscription
token through the gateway (the operator's Anthropic refresh token had expired; the
header shape is from the public clients and marked unverified); the snapshot run
against Spaces; the hub's replica on the homelab cluster, which arrives with the next
release; the work-side README import.

Exit criteria: `notebook` can be archived, and the two orientation files stop drifting
because there is only one source.

## Phase 5: Ledger, phases, metrics

- [x] Work item store: hash IDs, dependency edges, `discovered-from`. The `work_item`
      table replicates through the `sync` crate like documents; ids are
      `sha256(channel, project, site, created_ms, title)` so two nodes mint offline
      without collision; `deps_json` holds the edges, `discovered_from` and
      `discovered_by_session` the origin. `tracon work add|ls|ready|show|close|dep|rm`,
      `/api/work`, and the Work screen.
- [x] Ready-work query, deterministic topological sort. `tracon_sync::work::status`:
      Kahn over the open subgraph with ids as the tiebreak, then
      `(priority desc, created_ms asc, id asc)`; unknown deps block and say so; cycles
      block every member. Every replica lists the same order.
- [x] Session requires a work item; ready-work injected at start; session ends at item
      close. Plan and execute sessions are refused without a ready, unheld item (422 with
      the blocker named); the orientation carries the item and the project's ready work;
      `work_close` (or a close from the interface, or a publish) ends the holding session
      once its turn is over, reason `item_close`.
- [x] Plan / execute / review as separate spawned sessions. The operator starts plan and
      execute; the plan session ends by writing `plan-<item>` (`doc_write` to that one
      slug is not asked); the node spawns the review session at submit.
- [x] Model required in every session spec, validated at spawn. Since Phase 1; the spec
      now also names the phase, and a review session's model comes from the channel's
      `phases.review.model` binding — nothing is inherited.
- [x] Per-session budget enforced by killing the session. Since Phase 1; the default now
      comes from the channel's per-phase binding before the node's.
- [x] Deterministic supervision between phases. At `submit_review`, after the diff is
      captured and before anyone reads it, the node runs the worktree's `.tracon/checks`
      (else `[supervision] checks`, default `just check`) in a throwaway harness
      container with the worktree mounted and nothing else. A failure refuses the
      submission with the exit code and tail; the passing list is recorded on the review.
      `just analyse` / `just test` are whatever the project's file names.
- [x] Review sessions run with fresh context: requirements and diff only. A
      `review`-phase session on a fresh worktree at the reviewed commit, offered only
      `recall`, `doc_read`, `doc_search`, `review_verdict`; its verdict lands on the card
      for the human, who still decides.
- [x] Diff size cap at submit. `[review] max_diff_lines` (800) and `max_files` (40);
      over either is refused before the checks with "split the change".
- [x] Execute phase gated on a plan artifact. Refused with 422 until the item carries a
      plan document; `phases.execute.requires_plan=false` on a channel is the recorded
      bypass.
- [x] Metrics rollups: approvals per accepted change, cost per accepted change.
      `GET /api/metrics`, `tracon metrics`, the Metrics screen: approvals (permission
      answers + review verdicts) and gateway tokens per accepted change, priced only where
      `[providers.<p>.price]` is set. As seen from the node asked.
- [x] Per-channel daily cost ceiling enforced by the node. Binding
      `ceiling_tokens_per_day`, gateway tokens since local midnight: new sessions refused
      with 429 and the figures, and the gateway refuses every call at the ceiling so a
      running session stops spending. Meters on the Nodes screen.
- [x] Provenance queryable per commit: model, prompt, approval, policy version.
      `GET /api/provenance/{sha}`, `tracon provenance <sha>`: the review, the item and
      its plan, the implementing and review sessions with their models and the policy
      version they started under, the prompts, the approvals, the checks that passed.

Phase 5 implementation completed 2026-08-28, in eight PRs after a reviewer's security
patch (#52). Everything is tested in-process against the fake harness and a real
worktree; the two-node run of the ledger and a real plan → execute → review chain
through the gateway wait on the next release and a connected provider. Not built,
recorded so it is not assumed: hub-side rollups (metrics are computed on the node from
its own usage table and the mirrored sessions); mid-turn ceiling enforcement (the gateway
refuses the next call, the harness surfaces the error, the operator decides); a cost in
dollars for subscriptions (tokens are the unit; a price is per provider, optional).

Exit criteria met: a session's model is on its own spec and a review session's model is
a channel binding, so there is nothing for a subagent to inherit; and
`tracon metrics` prints tokens per accepted change.

## Phase 6: Clients

- [x] PWA: manifest, service worker, no local replica, no keys at rest. The worker
      caches the shell and the hashed assets and refuses to touch `/api` — a cached
      queue you cannot act on is worse than one that says the node is unreachable,
      and passing an endless SSE response through a fetch handler breaks it. Tested
      by loading the worker against stubs rather than through a browser, so the
      no-`/api` rule is checked on every PR.
- [x] Notification sinks bound per channel (pager for personal and client). The task
      reads the bus rather than the manager, which is what makes a *peer's* approval
      reach the phone: mirrored state is published untapped, and `publish_untapped`
      never reaches `Manager::publish_queue`. Hooking the manager would have missed
      exactly the case this phase exists for.
- [x] Tauri desktop wrapper: tray, command-tab, global hotkey, system notifications.
      Its own cargo workspace, so the node and hub stay buildable on a machine with
      no webkit or gtk headers.
- [x] Node supervised by the platform: `tracon service install` writes a systemd user
      unit or a LaunchAgent and starts it. Host-side CLI only, never a tool, so a
      session that breaks the build cannot restart the node that gates it. Coder
      needed nothing — there the container lifecycle already is the supervisor.
      Switchboard installed nothing on Linux; its units are a macOS-only migration.
- [x] Work approvals surface in the wrapper tray, with a kill switch one level down
      because killing is destructive and easy to mis-click. The tray does not stream
      output; anything worth reading opens the window at that route.
- [x] `@codemirror/merge` editable diffs feeding `/revise`. An edit is a request for
      changes carrying a patch, never an approval of something the operator altered:
      the agent applies it and resubmits, so the agent stays the only writer to the
      worktree. Lazy-loaded, so the phone never pays for the editor.
- [ ] Retire Switchboard, after supervision has moved and notebook and review are gone
- [x] Rehome or deliberately kill the display-linked theme switcher — **killed**, see
      `reference/phase-6-notes.md`. It is a macOS-only behaviour that nothing in
      tracon depends on, and rehoming it means a System Events code path in the
      wrapper forever.

Reaching a node from off its own machine needed an answer this phase did not have.
The corpus said the phone "reads over the hub" and also that "the hub never talks to
a browser", and the operator API had no authentication at all — only a loopback Host
check. So the node grew one: an operator token exchanged for an HttpOnly cookie, with
loopback unchanged, and the lab node reached through an ingress. The hub-mediated path
stays unbuilt and unneeded; documented under Clients.

Not built: the wrapper is not bundled or signed, and macOS is not in CI — the plist
and the launchctl path are written but built only on a Mac. Hub-side rollups and the
phone's read-over-the-hub path remain later phases.

Exit criteria: a task can be directed from the phone with both laptops closed, and
`switchboard` can be archived. The first half is built and waits on the release and
the cluster deploy; the second is the operator's, after the mac migration.

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
- **Model-proxied credentials.** *Un-deferred 2026-08-28:* the objection was TLS
  interception; a header-injecting gateway on the internal network is not that, and it
  is the only enforcement point for provider bindings. Now the first Phase 4 item.
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
| A node cannot establish its boundary | Every host must pass the startup check; the current Coder topology is deferred until Phase 3 rather than degraded |
| Corpus lock-in | Boring schemas, plain-text export for every kind; vectors deferred |
| Cost runaway | Per-session budget and per-channel daily ceiling, enforced not monitored |
| Retiring Switchboard too early | It retires last, after supervision has moved |
