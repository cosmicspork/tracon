# OpenCode v1.18.30 — Gate A compatibility manifest and verdict

Gate A of the OpenCode migration (see `docs/ROADMAP.md`, "OpenCode as the primary
harness"): candidate inventory and contract. Three source inventories were made
against the pinned release and one live probe of the pinned binary on a
network-isolated host. **Verdict: no stop condition. Proceed to Gate B.** Nothing
found requires a long-lived fork; every gap has a node-side or config-side
mitigation, listed below with the evidence that settled it.

## Pinned candidate

| Item | Value |
|---|---|
| Upstream release | `anomalyco/opencode` tag `v1.18.30`, commit `3104c14`, published 2026-09-09 (latest stable at inventory time) |
| `opencode-linux-x64.tar.gz` | sha256 `55007246858165496ff85ba1c2b648f7421e8e2013bf4189a680c9ff8e699d17` |
| `opencode-linux-x64-musl.tar.gz` | sha256 `a4eb84374d4f262ac46fe8b25c9e193a54a6f40f44bbe155a29c9c6309a34a5e` |
| `opencode` binary (from the glibc tarball) | sha256 `87bd160e053af86b5b409daabf71f8dc05bbc3a2a3a5f563f36011cdf706a999`, 184 MB, dynamically linked ELF; the tarball contains this one file with the web UI embedded |
| `opencode --version` | `1.18.30` (bare string); `GET /global/health` → `{"healthy":true,"version":"1.18.30"}` |
| API snapshot | `openapi-v1.18.30.json` (`GET /doc`, 162 paths, 188 method/route pairs in `routes-v1.18.30.txt`) |
| Environment variables | `env-vars.txt` (85 `OPENCODE_*` names found in `packages/{opencode,core,server}`) |
| UI asset/build digest | `opencode-ui-v1.18.30`: tree digest sha256 `348cb604b71e6f4706f3c5ee43d0f2ff44f01fce9cd8c8623b1759d9334e5ae7`, 951 files, 36,049,284 bytes (sourcemaps dropped). Tarball `opencode-ui-v1.18.30.tar.gz` sha256 `782ca629c49b1e2b620460b90c4d8ec7b1a2ad9bdc9783ba9227ad626cfb6696`. Built with `bun install --frozen-lockfile --ignore-scripts` then `bun run --cwd packages/app build` (bun 1.3.14, vite 7.1.4) by `containers/opencode-ui/build.sh`; digest checked in at `containers/opencode-ui/DIGEST` and recomputed by the node over the bytes it serves |
| Native UI route trace | to be captured in Gate D against the served bundle |
| Provider support matrix | `providers.md` §8 |
| Mutation-policy matrix | `api-ui.md` §2 |

Checksums prove artifact identity, not publisher identity. The macOS and the
aarch64 tarballs were not fetched; add their digests when those runners are built.

## Live probe (this host, `unshare -rn`, sandboxed HOME and XDG)

- `opencode serve --pure --port 4096` listens on `127.0.0.1:4096` only. Default
  hostname is loopback; mDNS is off unless `--mdns` and is refused on loopback.
- Serves `/doc`, `/global/health`, `/config`, and the UI index (`/`, 2.9 KB) from the
  binary. The index references only relative assets (`/assets/index-*.js`,
  `/assets/index-*.css`, favicons, `site.webmanifest`); no upstream host appears.
- The only outbound call at startup is the models catalogue
  (`GET https://models.opencode.ai/api.json`), which fails cleanly offline and is
  disabled by `OPENCODE_DISABLE_MODELS_FETCH=true` with `OPENCODE_MODELS_PATH` as
  the pinned source.
- No telemetry, no update check, no plugin install with `--pure`.

## Findings that shape the design

| # | Finding | Consequence for tracon | Evidence |
|---|---|---|---|
| 1 | No server-side hook gates tool execution before it runs; permission requests fire only when OpenCode's ruleset says `ask`, and `allow` emits no event. The `permission.ask` plugin hook is declared but never invoked. | Enforcement is containment plus an **all-`ask` ruleset** written by the node, with policy evaluated at the gateway and replied as `once`. This reproduces today's decision ledger. | `api-ui.md` §4, §8 #9; `config-state.md` §4.6 |
| 2 | `"always"` replies persist a grant inside OpenCode (in-memory in v1, a DB row in v2); `PATCH /session/{id}` can rewrite the ruleset. | Gateway rewrites every `always` to `once` and records the broadening; `PATCH /session/{id}` is forbidden. | `api-ui.md` §8 #10 |
| 3 | The web-UI fallback to `app.opencode.ai` cannot be disabled; `OPENCODE_DISABLE_EMBEDDED_WEB_UI` selects it. Server CSP is `connect-src *`. | tracon serves `/*` itself from the pinned bundle, never proxies the catch-all, sets its own CSP, and denies egress. | `api-ui.md` §6, §8 #6–7 |
| 4 | The app persists the server password in cleartext in `localStorage` and bootstraps from `?auth_token=`. Auth is inert when `OPENCODE_SERVER_PASSWORD` is unset. | Run the UI same-origin behind the gateway with credentials injected server-side and no password in the browser; assert the password is set and an unauthenticated request is refused. Gate D verifies the empty-password path. | `api-ui.md` §8 #8, #14 |
| 5 | `?directory`, `x-opencode-directory`, and several body fields act as implicit authorization to any host path. | Gateway pins the directory on every request and inspects bodies; unknown routes fail closed. | `api-ui.md` §8 #11 |
| 6 | Global SSE streams have no durable replay (`id` is always undefined); per-session `event?after=` and `history?after=` do. | Ingestion anchors on the per-session sequenced streams plus snapshots; global streams are hints. | `api-ui.md` §3, §8 #15 |
| 7 | `POST /pty` spawns arbitrary commands with no permission check; the WS ticket is not owner-bound and CORS trusts any `localhost` origin. | Default-deny; PTY only as an explicit workspace-scoped capability with a gateway-minted, owner-bound ticket. | `api-ui.md` §5, §8 #12–13 |
| 8 | OpenCode reads no base-URL environment variables. | Provider base URLs go into the node-written config, not env. | `providers.md` §2.5 |
| 9 | Anthropic Pro/Max OAuth was removed upstream in 1.3.0. The Codex plugin installs its token-holding, URL-rewriting fetch only when an `oauth` record exists in the runner's auth store. | Both subscriptions flow through the gateway unchanged: the runner holds a placeholder and no OAuth record. Anthropic subscription **login** needs a client: `claude setup-token` in the pinned Claude Code image, run in the login helper context. Trap: a provider named `openai` with an OAuth record silently rewrites `/v1` to `chatgpt.com`; assert the invariant with a test. | `providers.md` §3, §8 |
| 10 | Usage omitted by a provider silently becomes zero; no estimator. | The gateway's on-the-wire count remains the budget source; Gate B proves non-zero counts for the self-hosted endpoint or marks it unmetered. | `providers.md` §6.3 |
| 11 | Catalogue: tracon declares models (`OPENCODE_MODELS_PATH`) rather than probing; no localhost auto-probe exists. | Retire the omp catalogue denylist and built-in-provider disabling with the omp adapter. | `providers.md` §5.2, §7 |
| 12 | Single-source config is env-plus-path, not native: `OPENCODE_CONFIG` + `OPENCODE_DISABLE_PROJECT_CONFIG` + per-session HOME and all four XDG dirs. `/etc/opencode` has no env control. | Hermetic image without `/etc/opencode`; claim "no ambient discovery given a hermetic image". | `config-state.md` §1, §9 row 1 |
| 13 | Nested `AGENTS.md`/`CLAUDE.md` are attached by the `read` tool with no flag. | Same as today's harnesses, which read them too. Accepted; instruction content grants no permission. tracon's orientation stacks on top as before. | `config-state.md` §2.2 |
| 14 | Duplicate skill names warn-and-overwrite nondeterministically; `skills.urls` fetches over HTTP with no flag; skill bodies become shell-interpolated commands. | The node's manifest builder rejects duplicates, forbids URL sources, and treats skill content as code. | `config-state.md` §3 |
| 15 | `OPENCODE_PURE` disables config plugins only; a pre-baked package cache resolves offline with no integrity check. | Image is the trust root; read-only config dir; network denial. | `config-state.md` §4 |
| 16 | LSP is off by default and downloads are disableable, but formatter auto-installs are not, and a DROP-style network hangs the first edit. `serve` installs no signal handlers, so LSP children orphan on stop. | Bake formatters, override their `command`; runner egress **REJECT**s rather than drops; PID namespace with an init reaps orphans. | `config-state.md` §6, §9 rows 5–5c |
| 17 | Two processes can open the same SQLite DB; an older binary opens a newer DB silently; no backup command. | Node-owned single-writer fencing, quiesce → `VACUUM INTO` backups, a recorded state-schema generation gating restore, no downgrade promise. | `config-state.md` §7, §9 rows 6b–7 |
| 18 | MCP remote with bearer works; tool-call timeout is the SDK's 60 s (docs say 5 s); progress notifications reset it; MCP tools are permission-checked. | Tracon's `/mcp/{session}` connects as today; the 20 s `review_status` cap can be raised deliberately. | `config-state.md` §5 |

## Minimum launch environment (settled by code)

See `config-state.md` §9.2 for the full list. In short: per-session `HOME`,
`XDG_{CONFIG,DATA,CACHE,STATE}_HOME` (config dir writable-but-empty), `OPENCODE_CONFIG`
pointing at a read-only manifest outside the worktree, `OPENCODE_DB`,
`OPENCODE_DISABLE_PROJECT_CONFIG`, `OPENCODE_PURE`, `OPENCODE_DISABLE_DEFAULT_PLUGINS`,
`OPENCODE_DISABLE_EXTERNAL_SKILLS`, `OPENCODE_DISABLE_CLAUDE_CODE`,
`OPENCODE_DISABLE_LSP_DOWNLOAD`, `OPENCODE_DISABLE_MODELS_FETCH` + `OPENCODE_MODELS_PATH`,
`OPENCODE_DISABLE_AUTOUPDATE`, `OPENCODE_DISABLE_SHARE`, `OPENCODE_SERVER_PASSWORD`.
Image: no `/etc/opencode`, `rg` on `PATH`, LSP and formatter binaries baked and named by
absolute path, approved plugin packages pre-populated in the cache.

## Deny list for the API boundary (beyond deny-by-default)

`PATCH /config`, `PATCH /global/config`, `PUT|DELETE /auth/{providerID}`, `/provider/**`
auth routes, `POST /global/upgrade`, `PATCH /session/{id}`, `/experimental/**`,
`/tui/**`, `/sync/**`, `/mcp/{name}/**` writes, `*/share`, `POST /pty` unless an explicit
terminal capability is granted.

## What Gate B must prove live (not by reading source)

The provider half of this list is settled. `node/tests/opencode_providers.rs` holds each
item below; the live cases run the pinned binary and skip with a message when it is not
on the machine.

### Proven against the pinned binary on this host

| Claim | Where | What was observed |
|---|---|---|
| Provider traffic reaches the gateway and nothing else | `every_provider_call_arrives_at_the_gateway_on_an_allowlisted_path` | `POST …/model/anthropic/v1/messages` and `POST …/model/local/v1/chat/completions`, both on the shape's allowlist, both forwarded to the provider by the gateway with the real credential injected. The harness's own call, end to end — nothing in the test composes a request |
| Header bytes (§2.6, confirmed on the wire) | same, and `the_runner_presents_its_session_token_and_never_a_provider_key` | the binary puts the placeholder in `x-api-key` for the Anthropic shape and `Authorization: Bearer` for the OpenAI-compatible one — the header names §2.6 predicted, observed coming out of the binary — plus `anthropic-version: 2023-06-01` and the beta flags. The gateway swaps the placeholder for the credential under the same header, and the placeholder reaches no provider |
| OpenCode's own `anthropic-beta` survives the subscription merge (#166) | `the_subscription_shaping_merges_with_the_flags_the_binary_sends` | the binary sends `interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14`; the gateway forwards `oauth-2025-04-20` first and both of the binary's flags after, with `CLAUDE_CODE_SYSTEM` prepended to the system prompt |
| The only secret in the runner is its own session token | `the_runner_presents_its_session_token_and_never_a_provider_key` | what the binary presents is the placeholder and nothing else, in the header and in the body |
| Bun honours `HTTP(S)_PROXY` for `fetch`, and `NO_PROXY` on the gateway host is load-bearing | `bun_honours_the_proxy_and_no_proxy_exempts_the_gateway` | without the exemption the provider call arrives at the CONNECT proxy in absolute form (`POST http://…/model/anthropic/v1/messages`) and never reaches the gateway; with it, the call goes direct. Also observed: the binary reaches for `registry.npmjs.org` at session start with `--pure` and the default plugins off (§1.2), which the proxy allowlist is what stops |
| Non-zero gateway token counts for a self-hosted endpoint (finding 10) | `a_self_hosted_turn_is_counted_by_the_gateway` | a llama.cpp router, a real model, a real request composed by the binary: on one run 3057 prompt and 24 completion tokens counted by the gateway, equal to the figures the server itself reported |
| A provider that omits usage is unmetered, not free | `a_provider_that_omits_usage_is_marked_unmetered_not_free`, `model_gateway.rs` | the requests are recorded beside the zero, so the turn settles `unmetered` rather than as one that cost nothing |
| The declared catalogue is the whole catalogue | `the_binary_offers_exactly_the_declared_catalogue_with_egress_blocked` | with `OPENCODE_DISABLE_MODELS_FETCH`, `OPENCODE_MODELS_PATH`, and every route out pointed at a dead port, `GET /config/providers` offers exactly the declared models |
| The Codex trap is foreclosed by construction (finding 9) | `the_codex_shape_keeps_its_own_provider_id_and_arms_no_oauth_loader` | the provider id stays `openai-codex`, no provider named `openai` carries a Codex model, and the auth store holds no `oauth` record |

`--use-system-ca` / `NODE_EXTRA_CA_CERTS` turned out not to be load-bearing: the harness
reaches the gateway over **plain HTTP** on the runner's private network, so no certificate
authority is installed in the runner at all (`the_gateway_boundary_is_plain_http_so_the_runner_installs_no_ca`).
If that boundary ever becomes TLS, that test is where the CA handling has to be proved.

### Finding 19 — the session runner resolves its provider from the catalogue, not from `options.baseURL`

Found by running the binary the way the adapter drives it (`POST /api/session/{id}/prompt`).
That path resolves a model through the v2 catalogue and the protocol implementations in
`packages/llm`, not through the ai-sdk provider table §2.4 describes. A provider entry that
named the gateway only in `options.baseURL` was served from the **provider's own default
host**: a real `POST https://api.anthropic.com/v1/messages`, with the gateway, its
allowlist, its ceiling and its counting bypassed and no error anywhere. `api` — the
base-URL template (§1.1) — is the field this path reads, from the provider entry and from
`models.<id>.provider.api` in the pinned catalogue. The adapter now writes the gateway URL
into all three, and the test above fails if any of them is dropped.

The placeholder travelled with it: before the fix the request reached
`api.anthropic.com` carrying no credential at all and was refused there
(`x-api-key header is required`); with `api` written, the binary sends the placeholder
under the header its shape uses and the gateway authenticates the session. So the whole
chain works — harness to gateway to provider, credential swapped in the middle — and the
tests above assert it end to end rather than by replaying a captured request.

**Consequence for the plan.** §0's advice to "pin v1/ai-sdk behaviour" is sharper than it
looked: the two stacks disagree about *where a provider's requests go*, not only about how
usage is reported, and the adapter drives the newer one. Two things to carry forward,
neither of them a config-rendering matter:

- A release that changes which stack `POST /api/session/{id}/prompt` resolves through, or
  what it resolves a base URL from, moves the gateway boundary silently. Diff this on every
  upgrade alongside §2.4, and keep the live test above in the upgrade gate.
- The catalogue is populated asynchronously while the server is already answering. A prompt
  admitted before it settles is never run: the drain fails resolving the model and nothing
  retries, leaving the session on `prompted` with no step. The tests wait for the model to
  be listed (`GET /api/model`); the node's handshake does not, which is worth a look in the
  session-controller work.

### Still the operator's (no credential for them exists on a test machine)

- Hosted Anthropic and OpenAI API keys end to end.
- The Anthropic subscription: `claude setup-token` output lifted into the broker, and the
  gateway's shaping serving an OpenCode `anthropic` provider unchanged. The shaping itself
  is proven above against a broker credential of `oauth` kind; what is unproven is a real
  subscription token.
- The Codex subscription, and with it whether the ChatGPT backend accepts a Codex request
  whose system prompt stays in the message array.
- `OPENCODE_SERVER_PASSWORD` set and an unauthenticated request refused — the adapter sets
  it and the API-gateway tests cover the refusal; a live unauthenticated probe of a running
  session server belongs with the adversarial run.

## Files

- `api-ui.md` — server surface, route/event matrix, identity and replay, permissions,
  PTY, native UI, release artifact, stop/go.
- `providers.md` — provider config model, auth storage, subscription plugins, proxy,
  self-hosted, usage, catalogue, verdict table.
- `config-state.md` — config discovery, ambient repo content, skills, plugins, MCP, LSP,
  process and state layout, startup networking, verdict table.
- `openapi-v1.18.30.json`, `routes-v1.18.30.txt`, `env-vars.txt`, `checksums-linux-x64.txt`.

Paths inside the reports written as `<opencode-src>/…` or `packages/…` are relative to
the upstream checkout at the pinned tag.
