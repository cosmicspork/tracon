# Phase 4 notes: memory, documents, hub

Evidence and findings from building Phase 4. The plan's decisions (2026-08-28): the
model gateway is tracon's own, header-injecting, with the harness holding a placeholder
key; documents get a browser editor as well as CLI and agent tools; the hub becomes a
decrypting replica and processor for the personal and client channels.

## Spike: can the harness run on a placeholder credential? (2026-08-28)

omp 18.0.7 on the host, driven over ACP against a local listener that logged every
request (auth values redacted) and answered an Anthropic-shaped stream.

- Empty store + `ANTHROPIC_API_KEY=<placeholder>` + `ANTHROPIC_BASE_URL=http://127.0.0.1:47000`:
  34 models listed, `session/set_config_option model` accepted, `POST /v1/messages` (stream)
  with `Authorization: Bearer <placeholder>` AND `X-Api-Key: <placeholder>`, `anthropic-version`,
  `anthropic-beta: effort-…,context-management-…,interleaved-thinking-…`, `x-app: cli`.
  Turn completed with usage from the SSE (`message_start.usage`, `message_delta.usage`).
- `OPENAI_BASE_URL` is ignored for the `openai` provider (request went to api.openai.com, 401).
  `$HOME/.omp/agent/models.json` = `{"providers":{"openai":{"baseUrl":"…","apiKey":"<placeholder>"}}}`
  routes it: `GET /v1/models` then `POST /v1/responses` (stream), `Authorization: Bearer <placeholder>`.
- `omp --append-system-prompt=<file> acp` puts the file in the request `system` field.
- `OMP_STATE_DIR` does NOT relocate `agent.db` on the host; `$HOME/.omp/agent/` does. (Container: HOME=/root, mount at /root/.omp — unchanged.)
- Anthropic OAuth shape not captured: the operator's stored refresh token is expired
  (`invalid_grant`). Implemented from the public shape (`Authorization: Bearer`, `anthropic-beta: oauth-2025-04-20` merged); unverified until reconnected through the card.
- Side effects: two 5-token calls reached the operator's real codex provider from runs that used the real store before the HOME finding.

What follows from it: the gateway can inject for API-key providers today; Anthropic
needs both `Authorization` and `X-Api-Key` rewritten; OpenAI is routed by a materialized
`models.json` rather than an environment variable; and orientation content is delivered
by `--append-system-prompt`, not a file in the worktree.

## Sealed credential store

The broker's store is now `credentials.sealed`, XChaCha20-Poly1305 under a key derived
from the node's identity seed (`tracon/v0/credstore`, pinned in
`spec/vectors/key-derivation.json`). A plaintext `credentials.toml` found at startup is
sealed once and renamed `.imported`. Credentials carry a `kind` (`env`, `api_key`,
`oauth`) and, for model kinds, a `provider`; they travel to another node only when
pinned to it in `nodes`, as a direct-sealed `credential_handoff` frame — on enrollment,
or by `tracon credential share`. The receiver drops a row not pinned to it: the sender's
bindings are a claim, the receiver's are the rule.

## The model gateway

`/model/{provider}/{*path}` on the harness listener (the same forward the MCP surface
rides). The harness's placeholder key is its session token: `x-api-key` or
`Authorization: Bearer` names the session, the session names the channel, and the
channel's `bindings_json.providers` (when present) and the credential's own
`channels`/`nodes` decide before anything is forwarded. The upstream host must also pass
the egress allowlist. An `api_key` credential becomes `x-api-key` (Anthropic shape) or a
bearer (OpenAI shape); an `oauth` credential becomes a bearer with `oauth-2025-04-20`
merged into `anthropic-beta`. The response streams through untouched while a scanner
reads `usage` off it; every request is a `model_usage` row (`GET /api/usage`).

The harness is wired per session by `gateway::model::harness_wiring`: `ANTHROPIC_BASE_URL`
and the placeholder in the environment, and a read-only `agent/models.json` for the
providers omp only reaches through an override. The node's own model probe runs after
the listeners are up, presents a read-only probe token, and is skipped when no model
credential is usable on the node. `agent.db` on the harness volume is set aside at
startup (`agent.db.retired`), and `tracon harness import-credentials` / `harness shell`
are gone.

## Connecting a provider

`omp auth-broker login <provider>` prints the sign-in URL, starts a callback listener
on its own localhost, and then waits on stdin for the pasted redirect URL or code
(closing stdin crashes its readline, which is how the fallback was found). The node
runs it inside the boundary against a per-provider store at
`<state>/providers/<provider>/` mounted as the harness's `$HOME/.omp` — never a
session's volume — reads the URL off stdout, shows it on the Nodes screen, and writes
the paste-back to stdin. The callback port is unreachable from the operator's browser
by construction, so the paste-back is the path. When the login exits `0`, the node reads
`auth_credentials.data` (`{access, refresh, expires, accountId, email}`) from that
store's `agent.db` and puts it in the broker as an `oauth` credential pinned to the
node; `omp token <provider> --force-refresh` on a timer (half an hour ahead of
`expires`) refreshes it in the same store and the node lifts again. A provider without
a login flow (`openai` by default) is API-key only: `tracon credential import`.

## Record sync

Every replicated write (`document`, `memory`, `promotion`) goes through
`tracon_sync::write_change`: an HLC tick, the site's next `site_seq`, the row, and the
`change_log` entry in one transaction, then a `changes` frame on the record's channel.
Receivers apply row-level last-writer-wins on `(hlc_ms, hlc_ctr, site)`, dedupe on
`(site, site_seq)`, and tombstone deletes; a sequence gap, a retention `410`, or a
freshly received channel key asks each site for its own log after what is held
(`changes_request` / `changes_batch`, direct). Verified in `node/tests/sync_e2e.rs`: a
document written on one node reads on the other; with the hub answering 503, a fact
retained on A is recalled on A at once and waits in its outbox; concurrent offline edits
resolve to the later HLC on both sides once the hub returns; a delete across a second
outage tombstones on the other side; a node handed a channel key after the writes
backfills both sites' records.

Found on the way: **a hub outage deadlocked the Phase 2 mesh client.** `set_state_down`
called `presence_tick` from inside the watch channel's `send_if_modified` closure, and
`presence_tick` borrows the same watch — a self-deadlock on the first transition from
connected to unreachable, which no Phase 2 test exercised. Fixed by moving every side
effect of a state transition outside the closure. The test hub is taken down by a 503
gate rather than by aborting its accept loop: the nodes' keep-alive connections outlive
the loop, so an aborted hub still answers.

Bank identity is `sha256(channel ‖ canonical remote)` (`corpus::project`), resolved on
the node's side from `remote.origin.url` and recorded as `session.project_id`; a
repository without a remote falls back to its directory name and says so.

## Memory and documents as tools

Every session now gets the MCP server: `recall`, `retain`, `doc_search`, `doc_read`
need no credential, so the "no credential, no server" rule that held since Phase 1
gave way — the corpus is the node's own. Bundle v3 allows the four reads and `retain`;
`doc_write` is unnamed and therefore asked. What `retain` may make into context is
bounded by kind rather than by a question each time: a lesson, or a fact under 0.7
confidence, enters as `candidate` and waits for the nightly batch. `retain` refuses the
`directive` kind: those are the operator's, written through `POST /api/memories` or
`tracon memory add`. The CLI (`tracon doc …`, `tracon memory …`) is a client of the
running node's API on purpose — a write straight into the store would never reach the
mesh outbox. Documents keep the notebook's `If-Match`/412 edit contract.
