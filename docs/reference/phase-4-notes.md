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
