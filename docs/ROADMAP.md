# Roadmap

## Direction

- Personal agent workspace; one node is a complete installation. Mesh is optional.
- Enforce granted authority, not mandatory work items, phases, models, or review.
- Keep untrusted execution isolated and credentials outside agent-owned state.
- Prefer useful environments, clear evidence, and human intervention over more machinery.
- Record scoped decisions and supersede them explicitly; keep harnesses replaceable.

Completed items are removed from this file when they land; the changelog and the
reference documents under `docs/reference/` carry the history.

## Planned

### OpenCode as the primary harness

**Decision (2026-09-13):** OpenCode becomes the primary managed harness, driven through
its native server API, with its native web UI offered as an optional advanced interface.
Claude Code is retained as a second supported harness and as the Anthropic subscription
login client (`claude setup-token`); it has no native UI, so the advanced-view, PTY, and
remote-stream work is OpenCode-only. omp is removed at cutover: its adapter, image,
provider wiring, and catalogue workarounds. No new agent loop; the isolated execution
boundary stays. Planned flexibility must not weaken isolation.

**Release condition:** one real session per harness remains correctly supervised when
operated from either tracon or the OpenCode UI, in a browser and inside the desktop
app, locally and through the remote-node access path. A failed gate means no cutover.

Pinned candidate: OpenCode v1.18.30, inventoried in `docs/reference/opencode-v1.18.30/`
(compatibility manifest, route matrix, provider matrix, config and state findings, the
minimum launch environment, and the gateway deny list). The findings numbered below
refer to that manifest's table.

- [x] **Gate A — candidate inventory and contract.** Release pinned with checksums; API
      snapshot; server flags and startup egress verified live; provider, mutation-policy,
      and config matrices produced. Verdict: no stop condition.
- [ ] **Gate B — owner-side policy, credentials, and convergence.**
  - [x] OpenCode adapter: one isolated `opencode serve` per session with the settled
        launch environment, loopback bind, mDNS off, per-session HOME/XDG/DB, node-written
        read-only config, provider base URLs in config (finding 8), `OPENCODE_SERVER_PASSWORD`
        asserted (finding 4). Verified against a fake server and live against the pinned
        binary on a Linux host; the Podman and Kubernetes endpoint paths are exercised in
        the real-workflow runs of Gate F.
  - [x] Owner session controller and ingestion: durable identity mapping for session,
        message, part, permission, and PTY ids; reconciliation anchored on the per-session
        sequenced streams and snapshots (finding 6); uncertain-outcome handling for timed-out
        mutations. The node owns the durable sequence rather than the adapter's memory, so
        ingestion is keyed on (session, seq) and a re-delivered event — reconnect overlap,
        restart replay, snapshot race — produces no second tracon event; a child session
        the harness makes for itself is recorded with its lineage and surfaced as
        `untracked` rather than driven; a mediated mutation's intent is written before
        dispatch, a prompt that does not report leaves the session `uncertain` and refuses
        the next prompt until a snapshot settles it, and a permission reply is re-sent
        rather than asked about. Covered by tests against the fake server: duplicate
        delivery, restart mid-turn, a permission re-raised and one re-sent, a timed-out
        prompt cleared without re-sending, a child session, a session gone upstream, and a
        startup replay that closes the missed turn with its usage.
        - **Still open under this item:** no route yet to register a child session, so a
          fork or background subagent is only recorded and reported; PTY ids are mapped but
          nothing creates one until Gate D; and the startup path is exercised by test rather than
          by a real restart against a surviving `opencode serve`, which dies with its
          container today.
  - [x] Policy-aware API gateway: deny by default from the route matrix, directory pinned
        and bodies inspected (finding 5), all-`ask` ruleset with tracon deciding and replying
        `once`, every `always` rewritten and recorded, `PATCH /session/{id}` and the manifest's
        deny list refused (findings 1, 2). Mounted at `/api/opencode/{session_id}/…` on the
        operator router, so the operator's own guard — loopback or cookie, `Host`, and
        same-origin — answers first and the harness credential is injected on the node
        (finding 4). Covered by tests against the fake server: a readable route forwarded
        with Basic auth injected and no password reaching the client, every deny-list route
        refused with a 403 naming method and path plus a `gateway_refused` event and nothing
        sent, a foreign session id refused, the caller's `directory` replaced with the
        workspace in query, header and body, `always` rewritten to `once` and both the
        decision and the attempted broadening recorded, a prompt through the gateway running
        through the session manager rather than being forwarded, an unauthenticated and a
        cross-origin request refused before the gateway runs, and a PTY refused without an
        explicitly granted `terminal` capability.
        **Still to do here:** the PTY WebSocket ticket exchange (gateway-minted and
        owner-bound; Gate D) — the capability check point exists and the connect route
        answers 501 — and the native UI origin that will call this mount (Gate D). Child
        sessions (`fork`) and `init` are refused with a visible 403 until tracon registers
        them; a mediated call writes its `opencode_intent` row before dispatch, so one that
        times out is recorded as uncertain — on the intent and on the session — and left for
        the ingestion path to settle.
  - [ ] Provider proofs on the pinned binary: hosted API keys, the self-hosted
        OpenAI-compatible endpoint with non-zero gateway counts (finding 10), Anthropic
        subscription via `claude setup-token` lifted into the broker, Codex subscription
        through the gateway with the `openai`-plus-OAuth trap asserted by test (finding 9),
        Bun proxy handling and ai-sdk header bytes observed.
        - **Proven on the pinned binary** (`node/tests/opencode_providers.rs`, live cases
          skipped with a message where the binary or the local model server is absent):
          provider traffic reaches the gateway and only the gateway, on a path the shape's
          allowlist names; the header bytes on both legs (`anthropic-version` and
          `anthropic-beta` from the binary, `x-api-key` or `Authorization: Bearer` from the
          gateway, the placeholder in neither); the binary's own `anthropic-beta`
          flags surviving the subscription merge with `oauth-2025-04-20` first and the
          `CLAUDE_CODE_SYSTEM` prompt shaping applied; Bun honouring `HTTP(S)_PROXY` for
          `fetch` and `NO_PROXY` on the gateway host being load-bearing rather than
          decorative; the declared catalogue being the whole catalogue with the fetch
          disabled and egress blackholed; the Codex `openai`-plus-OAuth trap foreclosed by
          construction; and, live against a llama.cpp router, non-zero gateway token counts
          for a self-hosted endpoint equal to the figures the server itself reported, with
          a provider that omits usage settling as unmetered rather than billed as zero.
          `--use-system-ca` is not load-bearing: the gateway boundary is plain HTTP on the
          runner's private network, asserted as such.
        - **Found doing it** (manifest finding 19): the session path the adapter drives
          resolves a model's provider from the catalogue rather than from
          `options.baseURL`, so a provider entry without `api` was served from
          `api.anthropic.com` — with the gateway bypassed and without the placeholder,
          which is how a runner with no credential made a real request to a provider's own
          host. The adapter now writes the gateway URL into the provider entry, the
          catalogue provider, and each catalogue model, and both halves follow: the call
          arrives at the gateway carrying the session's placeholder. Carry forward that a
          release changing which stack that route resolves through moves the gateway
          boundary silently, and that the catalogue settles asynchronously — a prompt
          admitted before it does is never run, and the node's handshake does not wait.
        - The `claude setup-token` path is built and covered by tests: the Claude adapter
          runs it under a pty in a throwaway helper home, parses the sign-in URL out of the
          CLI's own screen, takes the pasted code, and lifts the printed `sk-ant-oat…` token
          as an `oauth` credential, so the gateway's existing subscription shaping (#166)
          applies unchanged. The `anthropic` login now resolves to the Claude adapter
          whatever `[harness] id` names. **Still unproven live:** no real subscription has
          been signed in through it, so the proof itself remains the operator's.
  - [x] Usage and spending accounting reconciled between OpenCode's per-message usage and
        the gateway's counts; unsent-text durability across disconnects. Every turn carries
        both numbers in one ledger keyed on (session, turn): the gateway's on-the-wire count,
        authoritative for budgets and ceilings, and the harness's own report
        (`tokens.{input,output,reasoning,cache.read,cache.write}` and `cost` for OpenCode,
        the ACP `usage` for omp/claude). At turn end they are reconciled — agreement within
        tolerance is recorded as such, disagreement writes a `usage_mismatch` event carrying
        both sides, and the harness's number can raise the charge but never lower it. A turn
        whose calls the gateway could not count (finding 10: OpenCode reports omitted usage
        as zero, and there is no estimator anywhere in the path) is marked `unmetered` and
        recorded with a `usage_unmetered` event rather than being charged zero; the channel
        ceiling reports those turns beside the day's counted spend instead of reading as a
        quiet day. Both numbers and the verdict are on the session API and in the SPA's
        session header. Unsent prompts are node-side per (session, operator), saved on a
        debounce, restored into the composer with a "draft restored" hint, cleared only by
        dispatching the prompt, and never delivered to the harness on their own. Covered by
        tests: the two sources agreeing, a harness under-reporting, a provider reporting no
        usage at all, gateway counts landing on the turn that made them, a ceiling enforced
        from the wire while the harness claims almost nothing, an unmetered turn flagged
        rather than passing silently, and a draft surviving a store reopen.
        - **Still open under this item:** the reconciliation is proven against the fake
          server and a stub upstream, not yet against the pinned binary and a live provider;
          `cost` on the v2 surface is always zero upstream (§6.2), so a priced turn is only
          as good as the v1 numbers the adapter reads.
  - [x] Adversarial API run: permission escalation, foreign session ids, config and auth
        writes, share, revert, shell, child sessions; interrupted SSE and killed processes
        during mutations; two simultaneous sessions cannot read or migrate each other's state.
        Every case asserts from both ends — what the client got back and what reached the
        harness — in `node/tests/opencode_adversarial.rs`. Against the fake: an `always`
        narrowed and recorded, `PATCH /session/{id}` and the saved-grant writes refused, a
        reply aimed at another session's permission refused on the identity map (the v1
        route names no session at all), every `{session}` route refused a foreign id, a
        path that does not normalise refused before it is classified, every spelling of
        `directory`/`cwd` replaced or refused, the config and credential routes refused
        with nothing sent, share/fork/init/agent refused, a revert decided by policy and a
        permitted one recording that the tree moved, a PTY default-denied, and a mediated
        mutation that never reported left uncertain and settled by reconciliation without
        a second answer being sent. Against the pinned binary: two sessions side by side
        with distinct XDG trees and databases, neither server's session list holding the
        other's and neither holding a lock on its own database (finding 17 observed, which
        is why the node now fences); the gateway refusing one session's mount with the
        other's id; stopping one leaving the other running; a refused PTY spawning nothing;
        an auth store that stays empty and a `GET /config` that is the node's; and a killed
        `opencode serve` leaving the mutation's intent uncertain with its reason rather than
        lost. The node-owned single-writer fence over a session's state directory (row 6b —
        upstream provides none) is built and covered.
        - **Not exercisable model-free, so still the operator's live run:** that a second
          identical tool call asks again after a gateway-mediated `once` (it needs a model
          to call a tool at all — what is proved here is that no grant can be saved and the
          ruleset stays all-`ask`); a subagent child session raised by the harness itself
          (the ingestion path for it is covered against the fake); and killing the server
          *after a tool call was recorded*, for the same reason.
- [ ] **Gate C — customization and recovery.**
  - [x] Launch manifest: a node-owned `LaunchManifest` per channel — skills, instructions,
        agents, the approved plugin list, the toolchain profile's LSP and formatter names,
        the effective provider set and the policy revision — with a content digest and a
        revision counting the times it changed, recorded on every session as
        `session.manifest_digest` and shown on the session header. `tracon skill import
        <dir|dir#git-rev>` and `/api/manifest` copy a package into node-owned storage and
        record its source, digest and the warning that skill content is code (finding 14: a
        body's shell interpolation is a slash command OpenCode executes). Duplicates refused
        at manifest build, URL sources refused outright, absolute and `..` paths and symlinks
        refused at import; `skills.paths` names one read-only root outside the worktree
        through `{env:TRACON_SKILL_ROOT}` and `skills.urls` is never written. A new revision
        never changes a running session: the files were staged at launch, and the digest
        still resolves to them afterwards. In `node/src/manifest/` and
        `node/tests/manifest.rs`.
  - [x] Plugins and tools only from an image-baked cache: the manifest approves a plugin only
        when the image's toolchain profile seeds it, and any other name is refused at build
        naming the cache path it would have needed
        (`$XDG_CACHE_HOME/opencode/packages/<pkg>@<ver>/node_modules/<pkg>`, §4.4). Against
        the pinned binary, a planted `.claude/skills`, `.opencode/skills` and
        `.opencode/tool` are absent from `GET /skill` and the tool listing while the
        manifest's own skill is present (findings 12, 15). Nested `AGENTS.md`/`CLAUDE.md`
        accepted and documented in `docs/ARCHITECTURE.md`, "The launch manifest"
        (finding 13). *Image half:* the pinned `@opencode-ai/plugin` is baked and seeded into
        each session's configuration directory and package cache before the harness starts,
        so the install OpenCode runs regardless of `OPENCODE_PURE` short-circuits offline.
  - [x] LSP and formatters baked and named by absolute path; runner egress rejects rather
        than drops; PID namespace with an init reaps orphans (finding 16); status shown.
        Profile revision 1 (`containers/harness-opencode/toolchain.json`): rust-analyzer
        and rustfmt at the workspace's pinned toolchain, typescript-language-server with
        TypeScript, and prettier; every other builtin server and both remaining
        self-installing formatters disabled by name. Refusal is measured rather than
        assumed, by `--deep` and by `node/tests/opencode_runtime.rs`, which also runs the
        pinned binary's `touchFile` path with downloads deliberately re-enabled and proves
        nothing of a killed container survives it. Kubernetes keeps the caveat below.
  - [x] State: node-owned single-writer fencing per session DB, quiesced `VACUUM INTO`
        backups, a recorded state-schema generation gating restore, no downgrade promise
        (finding 17).
        - Every OpenCode launch records the state's identity in `opencode_state`: the build
          the harness reported, the applied-migration ids read from the database's own
          `migration` ledger and a digest of them, the launch manifest digest (nullable until
          the manifest side records one), and where the state lives. `tracon session backup`
          refuses a live session unless `--quiesce` stops the harness through the supervisor's
          pause and stop — never a kill — then checkpoints WAL, `VACUUM INTO`s a copy,
          verifies it read-only with `PRAGMA integrity_check`, archives the rest of the state
          tree minus caches, and records a workspace commit/tree/dirty-digest checkpoint taken
          at the same instant. `tracon session upgrade-state --to` refuses without a backup of
          the current generation, refuses and changes nothing when the target build is not on
          the host, and migrates a `VACUUM INTO` clone under the target binary, swapping it in
          only if the clone's integrity check passes. `tracon session restore` refuses a newer
          build or a generation this runtime does not cover (row 7 — the older binary would
          proceed silently), refuses while the session runs or its fence is claimed, refuses a
          backup that no longer matches its own manifest digest, prints what it preserves and
          what it does not, and needs `--confirm` for a workspace that has moved since the
          checkpoint. Covered against the real pinned binary for the generation and the
          reopen, and against a stand-in build that wrecks the clone for "the original is
          untouched".
  - [ ] Legacy transition: archive omp sessions read-only with harness identity, reopen
        retained workspaces as new sessions with lineage, retire omp credentials deliberately,
        verify the omp adapter can no longer launch.
  - [x] Direct-harness recovery route documented for both harnesses; corpus export stays
        portable and vectors rebuildable. `docs/RECOVERY.md`: get the work out, run `claude`
        or `opencode` yourself, import back as a session — with what is lost (audit, budget,
        review evidence for anything done outside) stated plainly, and stated as a recovery
        route rather than a second supported managed harness. Node state recovery names the
        seed, the sealed broker, the policy bundle, and what a fresh node needs. Proved:
        a document export imports into a node that has never seen it, bodies byte for byte
        and kind, title, archived state and hash unchanged; the vector index deleted outright
        and rebuilt by `tracon doc reindex` returns the same top-k for the same queries; a
        session package reads back off disk with `tracon session show`, no node running.
- [ ] **Gate D — browser, desktop, and installed mobile PWA.**
  - [ ] Dedicated UI origin served by tracon from the pinned bundle, catch-all never proxied,
        tracon CSP replacing `connect-src *` (finding 3); short-lived single-use bootstrap
        exchanged for an HttpOnly cookie; no password in the browser (finding 4); Origin/CSRF
        on mutations and WebSocket upgrades.
  - [ ] Desktop: an unprivileged window with no node-management commands; navigation limited
        to the UI origin.
  - [ ] Installed mobile PWA on the always-on node: in-scope shell, isolated native view,
        third-party storage blocked, background/resume recovery, notification deep links.
  - [ ] PTY only as an explicit workspace-scoped capability with a gateway-minted owner-bound
        ticket (finding 7).
  - [ ] Native UI route trace captured and unknown mutations shown to fail closed.
- [ ] **Gate E — remote-node parity.** Bounded encrypted owner streams over the hub for
      HTTP, SSE, and PTY: authenticated stream ids, owner binding, flow control, reconnect
      without replaying input, revocation, protocol mismatch refused; hub sees ciphertext and
      routing metadata only.
- [ ] **Gate F — release and clean cutover.** Real Podman and Kubernetes project workflows
      on both harnesses; compatibility manifest promoted through a tracon release; omp
      removed; README, architecture, and design reconciled (harness sections, optionality of
      work items, phases, review, memory, remote access, and mesh); the migration plan archived.

**Upstream contributions worth a bounded PR** (not blockers): a flag that turns the
web-UI fallback into a 404; a flag check on nested instruction attachment; invoking the
declared `permission.ask` hook; an LSP status event.

### Still waiting on a real run

- [ ] Exercise a real private repository through preparation, agent work, checks, and
      authorized publication. (Needs a model credential and a private repository on the
      node; folds into Gate B's real coding task.)
- [ ] Exercise QA deploy and browser verification against a real target. (Needs a
      `glab` credential and a configured QA target. The Kubernetes backend has no scoped QA
      egress gateway yet, so this is Podman-only until it does.)

## Current limitations

- No real private-repository end-to-end run yet; see above.
- macOS releases are unsigned by choice — the publisher holds no Apple Developer ID —
  and are authenticated by GitHub build provenance instead, so Gatekeeper asks once on
  first open; the workflow still signs and notarizes if credentials are ever configured.
- QA deployment and browser verification are implemented but not exercised against a
  real target.
- The Kubernetes runtime backend has no scoped QA browser egress gateway;
  `scope_qa_egress` always refuses on that backend.
- The Kubernetes runtime backend drops denied egress rather than refusing it. A
  NetworkPolicy has no reject verb — it is a drop by construction — and no portable
  CNI option turns one into an ICMP or RST refusal, so the property Podman gets from
  having no route out has to come from the cluster there. Until a cluster that can
  express it is configured, a harness pod's protection against the hang is the
  download flag and the explicitly disabled server list alone, which cover OpenCode
  but would not cover a future harness with its own timeout-free fetch. The node
  states this rather than claiming the property: `check-boundary --deep` measures the
  same bound on both backends and fails the Kubernetes one if it drops.
- The OpenCode harness image is about 1 GB, of which roughly a third is the
  `librustc_driver` and LLVM shared objects that rust-analyzer and rustfmt link
  against. The dist channel publishes no self-contained build of either, and a
  rustfmt from anywhere else formats differently from the workspace's pinned one, so
  every edit would arrive with churn CI rejects.
- Harness-specific surfaces that will be rewritten at cutover: the omp provider wiring,
  catalogue denylist, and built-in-provider disabling; the retry-notice recogniser; the
  20 s `review_status` wait cap sized for omp's MCP client.

## Deferred

- **Retrieval reranker:** when existing retrieval proves insufficient.
- **Client terminal:** superseded by the OpenCode PTY capability in Gate D; no separate
  tracon terminal.
- **`cr-sqlite`:** only if real multi-writer convergence requires it.
- **Stacked MR automation:** decide whether stacks are preferable to feature flags first.

## Out of scope

- Multi-user tenancy or a team product.
- A model/agent loop inside the node.
- A general-purpose multi-harness framework: two concrete adapters behind one trait,
  not a plugin system for harnesses.
- Forking and maintaining the OpenCode UI.
- Required tracon configuration files installed into project repositories.
- A full IDE, general file editor, or per-project editor configuration.
- Business-domain features such as invoicing and billing.
- Arbitrary host execution through the node API. Service/CLI installation and node
  restarts remain explicit desktop/CLI operations; file imports use a picker/upload flow.
