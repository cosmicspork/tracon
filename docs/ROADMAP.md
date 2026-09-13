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
          fork or background subagent is only recorded and reported; usage reconciliation
          against the gateway's counts is the next sub-item; PTY ids are mapped but nothing
          creates one until Gate D; and the startup path is exercised by test rather than
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
        them; a mediated call that times out is recorded as uncertain and left for the
        ingestion path to reconcile.
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
          a provider that omits usage marked unmetered rather than billed as zero.
          `--use-system-ca` is not load-bearing: the gateway boundary is plain HTTP on the
          runner's private network, asserted as such.
        - **Found doing it, and blocking the rest** (manifest finding 19): the session path
          the adapter drives resolves a model's base URL from the catalogue rather than
          from `options.baseURL`, so a provider entry without `api` was served from
          `api.anthropic.com` with the gateway bypassed. The adapter now writes the gateway
          URL into the provider entry, the catalogue provider, and each catalogue model.
          The runner still has no way to present its placeholder on that path, so the
          gateway refuses its model call and no OpenCode turn completes through it — which
          leaves the hosted end-to-end runs and the usage reconciliation below waiting on a
          decision between driving the v1 session surface and giving the runner a v2
          credential.
        - The `claude setup-token` path is built and covered by tests: the Claude adapter
          runs it under a pty in a throwaway helper home, parses the sign-in URL out of the
          CLI's own screen, takes the pasted code, and lifts the printed `sk-ant-oat…` token
          as an `oauth` credential, so the gateway's existing subscription shaping (#166)
          applies unchanged. The `anthropic` login now resolves to the Claude adapter
          whatever `[harness] id` names. **Still unproven live:** no real subscription has
          been signed in through it, so the proof itself remains the operator's.
  - [ ] Usage and spending accounting reconciled between OpenCode's per-message usage and
        the gateway's counts; unsent-text durability across disconnects.
  - [ ] Adversarial API run: permission escalation, foreign session ids, config and auth
        writes, share, revert, shell, child sessions; interrupted SSE and killed processes
        during mutations; two simultaneous sessions cannot read or migrate each other's state.
- [ ] **Gate C — customization and recovery.**
  - [ ] Launch manifest: skill, prompt, and agent snapshots with digests; duplicates rejected
        and URL sources forbidden at manifest build (finding 14); read-only mount outside the
        worktree; a new revision never changes a running session.
  - [ ] Plugins and tools only from an image-baked cache; `.opencode/tool/` and project
        config suppressed (findings 12, 15); nested `AGENTS.md`/`CLAUDE.md` accepted and
        documented as matching today's harnesses (finding 13).
  - [ ] LSP and formatters baked and named by absolute path; runner egress rejects rather
        than drops; PID namespace with an init reaps orphans (finding 16); status shown.
  - [ ] State: node-owned single-writer fencing per session DB, quiesced `VACUUM INTO`
        backups, a recorded state-schema generation gating restore, no downgrade promise
        (finding 17).
  - [ ] Legacy transition: archive omp sessions read-only with harness identity, reopen
        retained workspaces as new sessions with lineage, retire omp credentials deliberately,
        verify the omp adapter can no longer launch.
  - [ ] Direct-harness recovery route documented for both harnesses; corpus export stays
        portable and vectors rebuildable.
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
