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
