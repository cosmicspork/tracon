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
  - [x] Dedicated UI origin served by tracon from the pinned bundle, catch-all never proxied,
        tracon CSP replacing `connect-src *` (finding 3); short-lived single-use bootstrap
        exchanged for an HttpOnly cookie; no password in the browser (finding 4); Origin/CSRF
        on mutations and WebSocket upgrades.
        `[ui] opencode_listen`/`opencode_url` bind a second listener (`node/src/http/ui.rs`)
        that serves the vendored bundle at `/` and routes the app's API calls into the #195
        gateway for the one session its cookie names. The bundle is built by
        `containers/opencode-ui/build.sh` from the pinned tag, is not in git, and is verified
        against the tree digest in `containers/opencode-ui/DIGEST` over the bytes about to be
        served — a tree that does not match is not served. "Open in OpenCode" mints a
        single-use 60-second capability bound to (operator login, session, this origin) and
        opens it in a **fragment**; an inline bootstrap tracon splices into `index.html`
        strips the fragment, exchanges it at `POST /boot` for a host-only HttpOnly
        `SameSite=Strict` cookie, and only then loads upstream's module — so no upstream code
        ever sees the capability. The operator cookie is not read on this origin and the UI
        cookie is not a credential on the operator's; both asserted. Every request re-reads
        the session and the operator login, so ending either revokes the window.
        **Proven in a browser** (`node/tests/opencode_ui.rs`, 18 cases plus a
        `TRACON_UI_SMOKE=1` harness driven with headless Chromium over CDP): against the real
        bundle and the pinned binary, the session page renders, **no request left the UI
        origin**, **no request carried an `Authorization` header**, and `localStorage` holds
        no password and no server record — which settles §8 #8's open question, that the app
        is usable with `password: undefined`.
        **Still to do here:** the PTY WebSocket upgrade is held to the same Origin rule but
        the gateway's connect route still answers 501 (the ticket exchange is its own row);
        `connect-src 'self'` will need the `wss://` form of this origin when it lands. The
        SPA's "Open in OpenCode" control (#205) was desktop-only — gated on `isTauri()`,
        because opening a *window* is the wrapper's to do — and the mobile shell row below is
        where a browser got its own way in, off the same mint route
        (`POST /api/sessions/{id}/opencode-boot`) and into an in-scope frame rather than a tab.
        That row also added a `framed` flag to `POST /boot`: the cookie this origin sets is
        third-party inside that frame, and `SameSite=Strict` would never arrive.
  - [x] Desktop: an unprivileged window with no node-management commands; navigation limited
        to the UI origin. A second window labelled `opencode`, declared in `tauri.conf.json`
        with `"create": false` and built on demand from a boot URL the node mints
        (`POST /api/sessions/{id}/opencode-boot`, feature-detected: a node without it answers
        404 and the operator is told the node does not serve that interface, rather than the
        call failing). Its capability (`wrapper/capabilities/opencode-window.json`) carries no
        `remote` section, so the UI origin it loads can invoke nothing at all — not this app's
        commands, not a plugin's — and holds only four local `core:window` controls.
        `wrapper/src/opencode.rs` decides every navigation in Rust: the UI origin is allowed,
        an `http(s)` link off it is handed to the system browser and refused in the window
        (so reaching the browser is not a capability the window holds), every other scheme is
        refused, and `on_new_window` denies a second window either way. The boot token travels
        in the URL fragment; a boot URL carrying one in the query, or sharing the node's own
        origin, is refused before the window exists, and the one line logged is redacted of
        query and fragment. Clipboard and `<input type=file>` need no plugin and none is added;
        the macOS Edit menu that makes copy and paste work is already installed app-wide.
        Covered by tests: the routing and boot-URL rules as unit tests, and a manifest test
        that parses `tauri.conf.json` and every capability file and asserts the `opencode`
        window is granted no `allow-desktop-*` command, that no other capability names it,
        that the opener is granted once and to the main window only, that every granted
        `allow-desktop-*` names a command `build.rs` declares, and that no `dangerous*` escape
        (`dangerousRemoteDomainIpcAccess`, `dangerousDisableAssetCspModification`) is set, so
        the CSP the window sees is the one the UI origin serves.
        Exercised live on Linux (webkit2gtk, offscreen X): the app launched against a
        stand-in node and a stand-in UI origin opened the window on the boot URL with the
        fragment intact and logged the URL without it; the page there was refused
        `desktop_restart_node` by Tauri's own permission check; a `file:` assignment and an
        off-origin `http` assignment both left the window on the UI origin, with the latter
        handed to the system browser; a real click on a `target=_blank` link opened the
        browser and no second window; a same-origin navigation went through.
        **Still to do here:** the same against the real UI origin, which lands in the row
        above — the wrapper calls `POST /api/sessions/{id}/opencode-boot` and that route now
        exists, so the feature detection should find it; an end-to-end run of the two
        together is unexercised. And the macOS leg — bundle, window behaviour and the Edit
        menu — which is the operator's to run.
  - [x] Installed mobile PWA: in-scope shell, isolated native view, third-party storage
        blocked, background/resume recovery, notification deep links.
        `/sessions/{id}/opencode` (`spa/src/routes/OpencodeShell.svelte`) is a route inside the
        manifest's `scope` that hosts the native view in a cross-origin `<iframe>` at the UI
        origin's boot URL, so an installed app never hands the session to the system browser.
        "Open in OpenCode" is no longer desktop-only: Tauri still opens #205's window, and
        every other client navigates to this route — never a new tab, which on a phone is the
        handover the route exists to prevent. The capability goes into the frame's `src` and
        nowhere else. `spa/src/lib/opencode.ts` refuses a boot URL whose token is in the query
        (where a log would keep it), whose origin is not the one the node named, or which
        shares tracon's own — the isolation *is* the origin — and `redact` is what every
        message about one goes through, so it reaches no history entry, error or console.
        No `sandbox`: a sandboxed frame's origin is opaque and the UI origin refuses
        `Origin: null` outright, so the attribute would break the boot exchange without
        tightening anything. No `postMessage` bridge; the status chip reads tracon's own
        session API, which this page is already authorised for. Nothing delegated by `allow`,
        and `referrerpolicy=no-referrer`. `frame-src` on the operator origin now names the UI
        origin alongside the preview origin, and the UI origin's `frame-ancestors` already
        names the operator's: both sides state the relationship and neither is assumed.
        **Third-party storage.** The shell is a page on tracon's origin and the view is a page
        on the node's other origin, so the cookie the view's own `POST /boot` sets is
        third-party — and third-party cookies are blocked by default on the phones this is
        for. `/boot` now takes a `framed` flag that the spliced bootstrap reports
        (`window.top !== window.self`, a throw read as framed), and a framed boot is answered
        with `Secure; SameSite=None; Partitioned` (CHIPS) where a window still gets
        `SameSite=Strict`. The loosening that `SameSite=None` would otherwise be is already
        answered by `origin_guard`, which requires a matching `Origin` on every mutation and
        every WebSocket upgrade; `Partitioned` then keys the jar to the *top-level* site, so
        the cookie exists only while tracon's own origin is the page around it — a stronger
        statement than `Strict` made, because it holds against a same-site attacker too.
        `Secure` is carried even on loopback, where this listener is plain HTTP: loopback is a
        potentially trustworthy origin and the browser accepts it, measured rather than
        assumed.
        **Proven in a browser** (`node/tests/opencode_pwa_shell.rs`, four cases; Chromium at
        390×844 with `--test-third-party-cookie-phaseout`, driven by
        `spa/tests/opencode-shell-driver.mjs`, skipped where the browser or the SPA's dev
        dependencies are absent): two genuinely cross-site loopback origins, `127.0.0.1` and
        `127.0.0.2` — distinct sites because neither has a registrable domain, where two
        *ports* of one host would not be, which is what an earlier attempt at this got wrong —
        with the real shell, the real mint route, the real bootstrap, the real cookie and the
        real gateway. The framed view authenticates with third-party cookies blocked and its
        mediated call returns 200; the shell's URL stays on the in-scope route and never
        carries the capability; there is exactly one frame and it is the other origin; the
        frame carries no `sandbox`, an empty `allow` and `no-referrer`; the cookie is absent
        from the origin's unpartitioned jar; and nothing overflows horizontally at 390px.
        The **control** is what makes that mean anything: the same flow with the `Set-Cookie`
        downgraded in the test's own middleware to what #205/#206 send is refused — 401 at the
        gateway, `refused:401` on the page — so the pair is cross-site and the partitioned run
        is not passing for a boring reason. A fourth case does the exchange on the wire with
        no browser at all, so a browser refusal is visibly the browser's.
        Upstream's bundle is stood in for and only there: it is 34 MiB, not in git, and not
        what any of this is about. The UI origin's `/` serves a two-line page with tracon's
        *own* `splice_bootstrap` applied, whose module makes one authorised call through the
        gateway and writes the answer where the driver reads it.
        **Background and resume.** On `visibilitychange`/`pageshow` the shell re-reads the
        session from tracon's API rather than trusting what is on screen. A session that ended
        while the app slept is said so, and the offer is the way back rather than a reconnect
        that cannot work — asserted in the browser with the session ended from outside the
        page between load and resume, so it is an ordering and not a race. A capability past
        the node's own cookie lifetime — returned by the mint route as `cookie_ttl_ms`, since
        an `HttpOnly` cookie cannot be read from the page — offers **Reconnect**, which mints
        a fresh one.
        **Service worker.** Cache key `v3`. `/api/**` and `/boot` pass straight through and
        are never cached — a replayed `/boot` response would be a capability the node has
        already spent — the UI origin is another origin and is never intercepted, and the
        previous version's caches are dropped on activate so an existing install updates
        without being reinstalled. All four asserted in `spa/src/lib/sw.test.ts`.
        **Deep links.** `notify::Kind::Opencode` and `Notification::opencode` put a push about
        an OpenCode session on the shell route, and the test reads `scope` out of
        `spa/public/manifest.webmanifest` itself — compiled in, so moving the manifest fails
        the build — rather than asserting about a string. Nothing produces one yet; the
        producer is the PTY and native-UI work in the rows below.
        **Still to do here:** the same run against the *real* pinned bundle, which needs
        `containers/opencode-ui/build.sh` to have been run on the machine — the browser case
        above stands in for upstream's JavaScript and asserts nothing about it. And the
        physical devices, which are the operator's: installing from the always-on node over
        HTTPS on iOS Safari and Android Chrome, and confirming the framed view authenticates
        with cross-site tracking prevention on. Safari is the open one — its partitioned-cookie
        story is not Chromium's, and if the frame is refused there the candidate fix is
        `document.requestStorageAccess` from inside the frame before `POST /boot`, which needs
        a user gesture and therefore a visible "Show OpenCode" control in the frame rather
        than a silent boot.
  - [ ] PTY only as an explicit workspace-scoped capability with a gateway-minted owner-bound
        ticket (finding 7).
  - [x] Native UI route trace captured and unknown mutations shown to fail closed.
        `docs/reference/opencode-v1.18.30/ui-route-trace.tsv` is 60 request shapes taken
        from a real browser: headless Chromium over CDP, the pinned bundle on the UI
        origin, the pinned binary behind the mediated gateway, and a fake upstream model
        behind the model gateway returning one short answer and one `bash` tool call. The
        tour loads the session page, types a prompt into the app's own composer, answers
        the permission that tool call raised, opens the changes view, switches model, opens
        settings, tries share, tries fork, opens a second session's URL, and asks for three
        paths nobody owns. Every row records method, path, status and the class that
        answered; `node/tests/opencode_route_trace.rs` re-derives every class from
        `http::ui::trace_case` and `gateway::opencode::trace_class` **in CI with no browser,
        no harness and no bundle**, so a route table or matrix edited without the trace
        agreeing fails there.
        **Nothing was unplaced.** Every path the app used is declared in the app-route table
        or classified by the matrix. The deny list the tour touched failed closed with a 403
        and a `gateway_refused` on the session's own record in every case: `*/share` both
        ways, `fork`, `init`, `PATCH /config`, `PATCH /global/config`, `PATCH /session/{id}`,
        `POST /pty` without the terminal capability — and a second session's routes, refused
        for naming another session's id. The four deliberately unowned paths (`/nope`,
        `/index.html`, a missing asset, a POST to `/nope`) were 404s, which is the claim that
        there is no catch-all made as evidence rather than as a reading.
        **Found doing it** (manifest finding 20): the app's only live channel is
        `GET /global/event`, which the deny list refuses because it has no durable replay
        (finding 6) — so the native UI renders, prompts and answers, but does not update
        itself, and the real permission the trace raised never appeared on the page. The fix
        is a node-synthesised, session-scoped `/global/event` served from tracon's own bus,
        not a hole in the matrix; it is the next row this view needs. The trace also settles
        that this build of the app is a **v1 client**: of the v2 surface it uses only
        `/api/health`, `/api/reference` and `/api/agent`.
        **Still to do here:** the bundle the trace was captured against is now installable
        rather than hand-built — `tracon setup` fetches `opencode-ui-v1.18.30.tar.gz` from
        the release and verifies it with the node's own loader, `--ui-bundle` installs it
        offline, `Dockerfile.node` carries it, and the release workflow builds it from the
        pinned upstream commit, asserts the tree digest, and attests it — but no release has
        published that asset yet, so the fetch path is exercised by test rather than against
        a real release.
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

### Session portability

Move a whole session to another node, or resume it later, without losing anything the
model or the operator saw. Today the durable record is the event ledger and, for
OpenCode, the per-session state backup (Gate C); the two gaps are the harness's own
context and the working files a session accumulates outside the workspace.

- [ ] Save the full-text conversation to a file before any compaction or context
      reset: every turn as the harness actually sent and received it (system prefix,
      prompts, tool inputs and outputs, model text), written to node-owned storage
      keyed by session and turn, so a compacted context is a summary of something
      that still exists rather than the only copy. Applies to both harnesses; for
      OpenCode the source is the durable per-session stream plus the message
      snapshot, for Claude Code the stream-json transcript.
- [ ] Save session scratchpads with the session: any scratch directory, notes, or
      temporary files the harness or the node created for the session outside the
      workspace tree, captured in the same package as the state backup and the
      transcript, with the same digest and generation record.
- [ ] Make the package the unit of transfer and resume: `tracon session backup`
      produces it, continuity transfer carries it, and reopening on another node or
      later restores transcript, scratchpad, state, and workspace checkpoint together,
      with lineage recorded and nothing silently dropped.

### Publication follow-through

A session or work item that ends in a PR or MR should not end at the push. The
publication record already carries the change's id (Gate B), the read-only
`pr_status`, `pipeline_status`, and `mr_status` tools are brokered, and `merge` is an
authority action that defaults to deny and binds to a target and revision; this is the
watcher and the button on top of them.

- [ ] Watch CI on the published change: the node polls the forge for check and pipeline
      status on the publication's PR/MR (brokered credential, bounded interval, backoff),
      records each transition as a session event, and shows the current state on the
      review and session cards and in the queue (pending, passing, failing with the
      failed job named, needs rebase). A failing run can be pushed to the operator as a
      notification, never treated as a question.
- [ ] A human merge button for the repository default branch: on green, the card offers
      Merge; the click is a revision-bound `merge` grant for that one PR/MR and the exact
      head commit the checks ran on, executed through the broker (squash or the repo's
      configured method), recorded like every other consequential action, and refused if
      the head moved, checks regressed, or policy denies merge for that channel. No
      auto-merge unless the signed policy allows it for the target.
- [ ] After the merge, close the loop: mark the publication merged with the merge commit,
      release the claim on the work item, and tidy the branch per the repo's convention.

### Still waiting on a real run

- [ ] Exercise a real private repository through preparation, agent work, checks, and
      authorized publication. (Needs a model credential and a private repository on the
      node; folds into Gate B's real coding task.)
- [ ] Exercise QA deploy and browser verification against a real target. (Needs a
      `glab` credential and a configured QA target. The Kubernetes backend has no scoped QA
      egress gateway yet, so this is Podman-only until it does.)

### Everyday work and portability

Build on the existing ledger, workspace snapshots, evidence, scoped grants, and
OpenCode gates above; do not introduce another agent loop or replace the store.
Order: prove ordinary work first; then continuity, environments, authority and
recovery; then reusable workflows and grouping. More autonomy is demand-driven.
This section records intended work, not additional guarantees of the current release.

#### Daily-use confidence and navigation

- [ ] **Prove the normal workflow.** Use the real-repository and QA runs above as the
      foundation, then exercise client disconnect/reconnect, interrupted execution,
      retained drafts, and recovery through completion. Record what actually ran,
      what remained uncertain, and where the operator intervened; fixture screenshots
      and fake-provider tests are not evidence of a live workflow.
- [ ] **Use the existing outcome metrics.** Judge changes by setup failures, time to
      verified work, interventions and tokens per accepted change. Keep unmetered
      usage and missing verification visible; add no agent reputation score.
- [ ] **Finish discoverability across surfaces.** Sessions/Usage navigation and consistent
      Settings naming have landed; keep them reachable in empty and archived-only
      states on browser, desktop and phone. Make export, import, handoff and recovery
      discoverable from the work they act on, rather than requiring knowledge of an
      endpoint or a second screen. Login and desktop setup remain lifecycle entry points,
      not extra navigation destinations. Browser access to the native OpenCode view
      remains part of Gate D, not a separate interface to maintain.

#### Node connections and desktop reliability

- [ ] **Give the Podman gateway an independent lifecycle.** On Linux, a gateway
      started by the setup API inherits the node service's process group and stops
      with it; startup only verifies the now-stopped gateway. Manage the gateway in
      its own user-service/cgroup rather than weakening the node's `KillMode` or
      rerunning full setup on every restart. Reconcile a missing/stopped gateway
      idempotently, retain fail-closed boundary checks, and reject incompatible
      gateway configuration with an explicit repair path. Prove that a node-service
      restart leaves the gateway running, stopped/missing recovery works, and node
      shutdown still cleans up harnesses.
- [ ] **Make credential handoff status converge.** A received share currently updates
      the broker without republishing the provider summary peers display. After
      durable receiver acceptance, recompute provider availability and publish the
      local stream and mesh summary, including clearing obsolete login failures.
      Distinguish queued, receiver-confirmed and usable credentials; the sender's
      node bindings and successful enqueue alone prove neither receipt nor use.
      Surface persistence/rejection failures without exposing values, and keep
      provider connection status distinct from runtime readiness. Prove receipt,
      persisted metadata and the peer view agree without a restart or another share.
- [ ] **Preserve deliberate sharing through OAuth renewal.** Keep explicit channel
      and recipient bindings when refreshing instead of resetting them to the local
      node. Define one refresh owner and propagate renewed copies sealed to the
      approved recipients; do not assume a broker handoff includes a harness's login
      database or let multiple nodes race a rotating refresh token. Show refresh
      failures, reconnect requirements and stale/offline copies. Define disconnect
      and recipient-removal behavior explicitly: removing local bindings is not
      evidence that a copied provider token was revoked. Prove renewal and reconnect
      preserve scope and update the receiver without granting any new authority.
- [ ] **Choose a policy for provider usage exhaustion.** Let the operator select in
      advance, per project/channel with a per-run override: pause and resume after
      reset; fall back to a named provider/model; or try that fallback, then wait if
      no approved provider is available. Default to pausing without an authorized
      fallback. Distinguish subscription/quota exhaustion from transient throttling,
      authentication failures and outages; a generic 429 proves no reset schedule.
      Use provider-reported reset evidence when available, otherwise show an unknown
      reset and offer manual retry or bounded rechecks rather than inventing a timer.
      Track cooldowns by the affected account/limit scope, including nodes sharing
      that credential. Record the selected policy, reason, effective model and next
      wake in the existing ledger; waiting survives restarts, releases idle execution
      resources, and remains cancellable. Fallback and automatic resume must recheck
      model/harness compatibility, destination data access, grants and spending caps;
      permission to wait is not permission to send context to another provider.
      Continue only from a recorded safe boundary, never replaying completed actions
      or treating an uncertain in-flight tool as complete. Prove fallback, reset-based
      resume and both-providers-exhausted behavior, including cancellation and unknown
      reset times, without duplicate execution or silent changes of harness.
- [ ] **Explain browser push enrollment failures.** Distinguish unavailable APIs,
      denied permission, service-worker failure, browser push-service registration
      failure and node subscription-storage errors. For the push-service case, say
      that registration failed, identify disabled/blocked push services as possible
      causes, and offer a supported browser or desktop notifications. Ungoogled
      Chromium is an example, not a diagnosis inferred from a generic exception or
      user-agent string. Keep technical details available without making them the
      only message; do not report a device as registered after failed enrollment.
      Verify the guidance at the failing stage without mislabeling permission,
      invalid-key or node errors as a browser transport problem.
- [ ] **Open external links through a clean Linux host launcher.** The shipped
      AppImage's bundled `xdg-open` silently skips KDE 6, and its inherited library
      path can break the Flatpak browser launcher. Select the host launcher and
      restore its host PATH/library environment for the child only; preserve the
      running app's libraries and existing URL/origin restrictions. Cover provider
      sign-in and native-harness external links, report observable dispatch failures,
      and never equate spawning a detached helper with opening the browser. Prove a
      harmless HTTPS link opens from the installed AppImage on KDE 6 with a Flatpak
      default browser; retain browser, mobile and other desktop behavior, and keep
      OAuth codes and full sign-in URLs out of diagnostics.

#### Review workspace and diff reading

Borrow interaction patterns from [cosmicspork/review](https://github.com/cosmicspork/review),
especially its [diff viewer](https://github.com/cosmicspork/review/blob/main/src/diff-part.ts),
prose editing and persistent feedback thread. Its renderer uses `diff2html` and
`highlight.js`; evaluate those against the existing CodeMirror dependency before
choosing an implementation. Keep Tracon's immutable candidates, isolated checks
and brokered publication: the reference tool deliberately leaves publishing to
the agent, which is not Tracon's authority model.

- [ ] **Side-by-side and unified diff modes.** Default to side-by-side on wide screens,
      with an explicit toggle and remembered preference; use unified as the narrow-screen
      default without removing the choice. Align changed lines and synchronize split-pane
      scrolling. Switching modes preserves the current file, reading position and feedback;
      neither mode requires entering the desktop-only diff editor.
- [ ] **Readable code and context.** Add old/new line numbers, syntax highlighting,
      within-line change emphasis, and wrap/scroll controls using Tracon's light/dark tokens.
      Expand context from the pinned base and candidate, never the live worktree. Label
      additions, deletions, renames, binary files and unavailable context explicitly; keep
      raw patch access and do not fabricate text for unsupported cases.
- [ ] **File navigation and review progress.** Use collapsible file sections on every
      surface, path navigation/filtering, per-file change counts/status, open/close controls
      and next/previous file or hunk actions. Offer a full-height/focused reading view rather
      than forcing the whole diff into one small scroll box. Track viewed files against the
      reviewed revision and invalidate affected progress on resubmission; viewed is not
      approved. Preserve keyboard navigation, focus and accessible control labels.
- [ ] **Keep large reviews responsive.** Paint file headers first, render visible/open
      bodies incrementally, and load highlighting lazily. Collapse generated and oversized
      files with an explicit reason and Load diff action; never silently omit them or
      count them as reviewed. Cache by content/revision and bound memory/DOM work; open-all
      must not freeze the interface. Rendering performance does not waive submission caps.
- [ ] **Review prose alongside code and evidence.** Present the outgoing title/body as
      readable, sanitized Markdown with source editing and a preview of the exact text
      to be published. Keep requirements, checks, demonstrations and diff easy to move
      between without burying the code below administrative detail. Preserve current
      narrative-report and evidence contracts rather than adding another artifact store.
- [ ] **Persistent, anchored feedback.** Add general and file comments like the reference
      tool, then true line/range threads bound to candidate revision, path and old/new side
      (the reference's file comment uses a `path:1` anchor, not a selected line). Return
      comments and suggestions through the agent review contract. Preserve the thread across
      resubmissions; resolved and outdated are distinct, and moved anchors must not silently
      attach to unrelated code. Render untrusted comments through the existing safe renderer.
- [ ] **Durable review drafts and re-review.** Preserve unsent feedback and title/body edits
      across navigation and reconnect, with explicit saved/conflict state. Keep desktop diff
      drafts revision-keyed; resubmission must not silently apply old edits to new code.
      Show what changed since the last reviewed revision alongside the complete base diff,
      retain decisions and feedback, and notify/deep-link to a ready-for-re-review item
      through the existing queue and push system. Suggestions still return to the agent
      to apply and resubmit; the review client never writes the worktree.
- [ ] **Explain and validate verdict actions.** Keep decisions reachable while reading
      long diffs. Request changes and Reject should open/focus a labeled reason composer,
      with the reason required at submission rather than an unexplained disabled button.
      Preserve feedback on errors. Explain genuinely unavailable actions inline (publishing,
      stale revision, missing evidence), distinguish rejection from requested revision,
      and refresh authoritative state after failed or concurrent decisions.
- [ ] **Separate review decisions from publication recovery.** Show credential/binding
      readiness before offering publication and link missing GitHub/GitLab access to the
      appropriate Connections settings, without exposing secrets or guaranteeing remote
      permissions from token presence alone. A failed publish needs a durable, visible
      outcome and remedy. Distinguish definitely-not-attempted, failed, in-progress,
      uncertain and published; reconcile uncertain external effects before retrying.
      Keep request-changes/reject available when safe and explain when they are not.
      Adding a credential never retries publication automatically, and changed revisions
      or outgoing prose require fresh authorization rather than inheriting an old approval.
- [ ] **Prove the review experience on real surfaces.** Exercise both modes in browser
      and desktop, narrow-screen reading and feedback, long lines, mixed file types,
      generated/large patches, light/dark themes and keyboard-only use. Include blank
      reasons, missing forge credential, publication failure/retry, reconnect with drafts,
      stale revision and resubmission with existing threads. Verify the exact approved
      revision/prose and the distinction between review state and external publication.

#### Work continuity and outcomes

- [ ] **Continue the work, not just the transcript.** A work-level continuation view
      carries intent, decisions, attempts, blockers, next action, the current workspace,
      execution lineage and evidence links. Offer continue, change approach, and abandon
      while retaining artifacts. Plain sessions can acquire this continuity without
      being forced into a work item or plan/review lifecycle.
- [ ] **Produce a concise outcome record.** Show what changed, what was verified, what
      needs a decision, unresolved or uncertain actions, and cost/usage. Derive revision,
      workspace, check and publication status from recorded state; any narrative summary
      is supplementary and cannot turn a claim into verification.

#### Project environments and effective authority

- [ ] **Save validated project setup profiles.** Extend the existing launch manifest and
      toolchain profiles rather than creating another customization system. Record the
      image, language tools, skills, dependency preparation/cache policy, required checks,
      and explicitly configured test services or previews for the projects actually used.
      Snapshot the configuration per execution; show its last successful validation and
      invalidate that assurance when its inputs change.
      Treat customizations as versioned source with provenance, compatibility
      requirements and validation evidence, not just a collection of settings.
- [ ] **Preview preparation and explain incompatibility.** Show detected ecosystems,
      supported preparation steps, missing tools and actionable remedies before launch.
      Preserve credential-free preparation and isolation; unsupported scripts, services
      or devcontainer features must not become silent host-execution exceptions.
- [ ] **Explain authority in task terms.** Before and during work, summarize the selected
      node, harness/image, effective access, applicable grants, spending limits and actions
      that still require approval. Derive this from current policy and grants, not a
      parallel permissions model. Explain node eligibility and placement; any suggested
      runner stays manually overridable within the eligible set.
      Show what a customization can read, change and send elsewhere, not merely
      which tools it requests.
- [ ] **Review, activate and roll back personal customizations.** Let an agent propose
      a skill, instruction package, project profile or recipe. Show source changes,
      provenance, required tools/data/actions and compatibility/validation evidence
      before explicit operator activation. Reuse Gate C's node-owned manifests and
      pinned artifacts; activation creates a revision, not a second configuration
      system. Updates never modify running sessions or silently broaden authority.
      Keep disable and rollback available; rollback cannot undo external effects.
- [ ] **Expose resource-scoped operations for custom work.** Start with operations
      demanded by real recipes through existing tools and policy enforcement:
      evidence for a named work item, not unrestricted ledger access; proposing a
      change, not general mutation rights. Types describe the contract but are not
      a security boundary: the node enforces scope and execution remains isolated.
      Installing an extension grants no raw credentials, privileged node access or
      unrestricted outbound networking. Prove out-of-scope access is refused.

#### Portable data and recovery

- [ ] **Publish a versioned data contract.** Extend the current signed candidate JSON,
      offline package reader and document export into round-trip session/corpus export.
      A standard archive containing a JSON manifest, JSON/JSONL records, Markdown and
      ordinary artifact files is the preferred shape; settle the container and schema
      before implementation. No opaque database dump or Tracon installation required
      for independent processing.
- [ ] **Specify complete portable content.** Preserve selected sessions, messages/events,
      work items and relationships, decisions, drafts, documents, memories, workspace
      state, evidence, attachments and provenance. Define identifiers, timestamps,
      encodings, paths and omission rules. Optional harness-native state must name its
      harness/build/schema compatibility; it is not the portable record's only copy.
      Include selected customization source, configuration and pinned revisions;
      imported customizations require fresh authorization and contain no credentials.
- [ ] **Make import predictable and independently implementable.** Publish schemas,
      representative exports, integrity/signature verification rules, version migration
      policy and examples for ordinary processing tools. Define duplicate/conflict
      handling and reference remapping; prove export/import on a fresh node and independent
      reading without the node. Retain old-format import compatibility through explicit
      migrations rather than losing existing archives.
- [ ] **Separate export, backup and handoff.** Portable data export omits credentials and
      private identity keys; warn that transcripts and workspace files can still contain
      secrets and make exclusions explicit. Full installation backup protects any
      deliberately included secrets separately. Neither importing data nor verifying a
      signature grants execution authority. Derived indexes remain rebuildable.
- [ ] **Unify recovery and maintenance entry points.** Build on Settings' maintenance
      controls, session-state backups and the existing recovery route. Show retained
      workspaces, checkpoint/export/backup age and destination, runtime/harness/node
      versions, what survives stop/restart, and actions to inspect, export, restore or
      diagnose. Keep client reconnect, execution recovery, workspace rescue and disaster
      recovery distinct. Exercise restore and upgrade failure recovery; promise rollback
      only where state compatibility permits it.

#### Cross-node session handoff

- [ ] **Move an unfinished session to another node.** Add a session-level action:
      select destination, check compatibility, checkpoint and transfer, then continue
      with a continuous work history and explicit execution lineage. Do not require a
      submitted candidate, mesh membership for file-based transfer, or an opt-in work
      item. This extends candidate sharing; it does not rename it.
- [ ] **Carry the actual continuation state.** Include unfinished workspace changes,
      conversation and decisions, selected context, unsent draft, next action,
      harness/model/manifest identity, evidence, usage and remaining limits. Reuse the
      portable contract and existing transport; support bounded transfer of realistic
      workspaces rather than assuming they fit a single mesh frame.
- [ ] **Transfer execution ownership safely.** Quiesce in-flight work and fence the
      source before the destination executes. Persist transfer state and acknowledgements;
      retries must not create duplicate sessions or repeat external side effects. Surface
      uncertain actions for reconciliation. A timeout or partition never silently enables
      both owners; failure and cancellation have explicit safe recovery paths.
- [ ] **Re-evaluate authority and state compatibility.** Destination policy, credentials,
      capabilities and budgets decide whether continuation can run; transport imports no
      authority or private keys. Compatible native-state resume and fresh-session
      continuation from portable context are distinct, visible outcomes. Explain any
      lost fidelity before confirmation; never claim live process migration or identical
      cross-harness state. Pending questions and permissions must be reconciled, not
      blindly replayed or treated as newly granted.
- [ ] **Prove handoff in both directions.** Exercise laptop-to-server and server-to-laptop,
      interruption during transfer, unavailable destination, duplicate import, dirty
      workspace, incompatible harness state and pending external action. Verify one active
      owner, retained work/draft/history, and no budget reset or authority widening.

#### Reusable work and bounded automation

The [extensible-software principle](https://jeremymorrell.dev/blog/extensible-software-in-the-age-of-llms/)
fits here as personal workflows around an accountable core, not a new platform.
Sequence: fix node/desktop/credential reliability; finish profiles and effective
authority; deliver reviewed recipe authoring and activation; prove one personal
customization; then add bounded triggers and sharing through that lifecycle.
Gate C's completed foundations remain complete. Provider fallback preferences may
be configurable, but quota classification, safe resumption, accounting and permission
enforcement stay in the core.

- [ ] **Saved workflow recipes.** Start with a few recurring procedures such as regression
      investigation, small changes and release preparation. Reuse phase presets and the
      ledger; snapshot each recipe on invocation, persist expensive checkpoints and
      external waits, and let the operator revise or stop a run. No mandatory workflow
      language or ceremony for a plain prompt.
      Let an agent draft and revise recipes from a request; the operator reviews
      and activates a pinned revision through the customization lifecycle above.
- [ ] **Prove one personal customization end to end.** Produce a preferred review
      summary when work is ready: intent, changed behavior, evidence, unresolved
      risks and decisions needed. Use an approved recipe, scoped evidence access
      and existing report/document surfaces. Prove usefulness on real work, source
      and revision visibility, safe failure without disrupting review or approving
      anything, and revision/disable/export/import without authority transfer.
      Start custom UI as isolated reports/views using document-preview isolation,
      not plugins in the administration UI. Expose no node session cookie, Tauri
      management commands or application store; any later interactive action needs
      a narrowly defined authorized operation.
- [ ] **Lightweight batches.** Group related work with dependencies, explicit assignment/
      ownership and reclaim rules, aggregate spending, blockers and one completion report.
      Build on current readiness and session holders; retain discovery lineage and prevent
      competing automated claims before scaling dispatch.
- [ ] **Integration when parallel same-repository work needs it.** Prefer a suitable forge
      merge queue over a custom merge agent. Serialize integration, pin the combined
      revision, and reverify rebased/combined changes; independently approved patches do
      not automatically approve their combination. Escalate conflicts instead of silently
      discarding work. This is demand-gated, not a prerequisite for single-session use.
- [ ] **Bounded scheduled/event-driven chores, after daily-use reliability.** Begin with
      low-stakes work that returns a report and stops. Set concurrency, spend, time and
      retry limits; preserve pause/stop, deduplicate triggers, and ask before consequential
      actions absent a valid scoped grant. Use ordinary supervisor logic for scheduling,
      reconciliation and health checks, not permanent agent roles.
      Run approved recipe revisions through the existing supervisor and ledger,
      never arbitrary extension code inside the node.
- [ ] **Optional conversational coordination, only if useful.** An ordinary managed
      session may inspect permitted cross-project work and propose actions through existing
      tools. No privileged manager loop in the node. Fresh reviewers remain useful for
      judgment, not replacements for deterministic checks, evidence or authority boundaries.

#### Independent installations

- [ ] **Make the personal workflow reproducible by another operator.** Provide one proven
      installation-to-first-task path, explicit OS/runtime/harness/login compatibility,
      understandable project preparation and actionable diagnostics. Build on existing
      setup and maintenance surfaces; keep remote access and mesh optional.
- [ ] **Bound maintenance expectations.** State supported versus experimental integrations,
      pinned-release/update policy, interruption and backup requirements, and the recovery
      path for a failed upgrade. Extend the existing compatibility gates instead of
      promising arbitrary drop-in harnesses or maintaining a native UI fork.
      Apply compatibility checks, failed-update recovery and explicit rollback to
      installed customizations as well as the managed runtime.
- [ ] **Offer a clearly labeled demonstration mode.** Reuse the fixture machinery to
      explore the workflow without credentials or containers, with demonstration data
      unmistakable and no implication that its output proves a live run.

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
- **General dashboard/plugin system:** only after isolated reports and views prove
  insufficient. No new extension runtime, marketplace or workflow language is required
  for the personal-customization work above.

## Out of scope

- Multi-user tenancy or a team product.
- Federated work marketplaces, reputation scores or agent organization charts as product goals.
- A model/agent loop inside the node.
- A general-purpose multi-harness framework: two concrete adapters behind one trait,
  not a plugin system for harnesses.
- Forking and maintaining the OpenCode UI.
- Required tracon configuration files installed into project repositories.
- A full IDE, general file editor, or per-project editor configuration.
- Business-domain features such as invoicing and billing.
- Arbitrary host execution through the node API. Service/CLI installation and node
  restarts remain explicit desktop/CLI operations; file imports use a picker/upload flow.
