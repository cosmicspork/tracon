# Phase 0 validation

Results of the roadmap's Phase 0 checks, run 2026-08-24 on the personal machine
(Bazzite, rootless podman 5.8.4, netavark, SELinux enforcing). Work-side checks that
need the Coder pod are listed at the end with the exact commands to run there.

## ACP session, end to end

`omp acp` (18.0.4) driven over stdio with a hand-written JSON-RPC client. Full log in
[reference/acp-omp-18.0.4-session.jsonl](reference/acp-omp-18.0.4-session.jsonl).

Shapes that matter for the adapter:

- `initialize` → `agentCapabilities` includes `loadSession`, `sessionCapabilities`
  (`list`, `fork`, `resume`, `close`), `mcpCapabilities` (`http`, `sse`). The auth
  method is "use existing local credentials under `~/.omp`", which matches the model
  auth decision.
- `session/new` → `sessionId` plus `configOptions` (mode: default / plan, model
  selectors). Mode and model are settable per session from the client side, which is
  where the spec-requires-a-model rule can be enforced without harness help.
- `session/update` notifications seen: `available_commands_update`,
  `session_info_update`, `config_option_update`, `tool_call`, `tool_call_update`,
  `agent_message_chunk`, `usage_update`. `usage_update` carries `used`, `size`, and
  `cost.amount` in USD per turn, so the budget meter is free from omp as well as Claude
  Code.
- `tool_call` carries `kind` (`read`, `execute`, ...), `locations`, and `rawInput`;
  `tool_call_update` carries `rawOutput`. Enough to render the session screen and to
  gate on `kind`.
- The agent calls back `fs/read_text_file` when the client declares `fs.readTextFile`.
  With it declared false, omp reads files itself. The node should declare false; the
  harness reads inside its own container and the node does not want to be a file
  server for it.
- `session/request_permission` offers `allow_once`, `allow_always`, `reject_once`,
  `reject_always` with the tool call attached. This is the gate's input.

Prompt round trip cost: ~20k input tokens for a one-line task, mostly cached. Session
startup injects a large system context; the budget meter must count it.

## Personal privilege boundary, by hand

Goal: harness in a container that can reach the model provider and the node, and
nothing else. Findings, in the order they were hit:

1. **`podman network create --internal` blocks the host too.** No default route at all,
   so the harness cannot reach a node listening on the host. "Node as the only route"
   cannot be done with an internal network alone.
2. **The fix is a node-owned gateway container on two networks.** The gateway sits on
   the default (egress) network and the internal one; the harness is on the internal
   one only. Verified: direct egress from the harness is unreachable, the gateway is
   reachable by IP, and the gateway has egress.
3. **Egress allowlist is an HTTP CONNECT proxy on the gateway.** tinyproxy with
   `FilterDefaultDeny` and an allowlist of provider hosts; harness gets `HTTPS_PROXY`.
   Verified: `api.openai.com` reaches (401, needs auth); `github.com` is refused by the
   filter. Config in [reference/gateway-tinyproxy.conf](reference/gateway-tinyproxy.conf).
   Bun-based harnesses (omp) honor `HTTPS_PROXY`.
4. **Harness reaches the node through the gateway, not a mounted socket.** A unix
   socket bind-mounted into the harness is denied by SELinux (`container_t` may not
   connect to an `unconfined_t` listener; `:z` and `chcon` do not help, only
   `label=disable` does). Instead the gateway mounts the node's socket with
   `label=disable` (acceptable: the gateway is node-owned and trusted) and forwards a
   TCP port on the internal network to it. Verified end to end.
5. **Create the internal network with `--disable-dns`.** Otherwise the gateway's
   resolver becomes the internal network's aardvark, which answers NXDOMAIN for
   everything outside the network and never falls through. Harness finds the gateway by
   `--add-host` with a fixed `--ip`.
6. **Unix socket paths must be short.** The 108-byte AF_UNIX limit; put the node socket
   under `$XDG_RUNTIME_DIR`.
7. **Harness container flags that worked:** `--cap-drop=ALL`,
   `--security-opt=no-new-privileges`, `--network <internal>`, no socket mounts.
   `label=disable` was needed only to run the omp binary from a bind mount in this
   experiment; a built harness image does not need it.

Conclusion: the boundary is achievable on the personal machine with rootless podman.
The node owns three things: the internal network, the gateway container, and the
harness container.

## Harness under restriction

omp run inside that boundary (`git` present, no `gh` / `glab` / `acli`, no `.env`,
origin set to an unreachable host) and asked to create a file, commit, push, and open a
PR. Log in [reference/acp-omp-restricted-session.jsonl](reference/acp-omp-restricted-session.jsonl),
driver in [reference/acp-drive-restricted.py](reference/acp-drive-restricted.py).

Behavior: created and committed the file; `git push` failed with the proxy's 403;
recorded the failure with the reason; checked `which gh`; reported exactly what
succeeded and what failed and why; stopped. No retry loops, no attempts to route around
the proxy, no hallucinated success. Every shell command went through
`session/request_permission` first.

Two things the node must handle that the run exposed:

- The harness set `git config user.name root` when it found no identity. The node
  materializes git identity into the worktree config at session start.
- The harness committed on `main` because it was told to. The worktree rule is
  enforced by the node creating the worktree and branch, not by the harness.

Conclusion: the harness tolerates restriction. The gate can be built around cooperation.

## Local devcontainers

All six `.devcontainer/devcontainer.json` files under `~/src` (blog, health-dashboard,
hounddogreading, jobs, kritee, laravel-rag-chat) have no docker-in-docker or
docker-outside-of-docker feature, no `docker.sock` mount, no `privileged`, no `capAdd`.
The work-side repos are not on this machine; see below.

## DigitalOcean serverless inference

From the Inference docs (models, pricing, batch inference, embeddings how-to):

- **Embeddings** are a plain serverless endpoint: `POST https://inference.do-ai.run/v1/embeddings`.
  Models and prices per 1M input tokens: Qwen3 Embedding 0.6B $0.04 (8k window,
  multilingual, public preview), BGE-M3 $0.02 (8192 tokens), E5 Large v2 $0.02,
  GTE Large v1.5 $0.09, all-MiniLM-L6-v2 $0.009, multi-qa-mpnet-base-dot-v1 $0.009.
- **Reranking**: only BGE Reranker v2 M3, $0.01 per 1M tokens, and it is exposed only
  as a knowledge-base option, not as a standalone endpoint. A reranker in the retrieval
  stack means running one locally or via another provider.
- **Batch inference exists only for OpenAI and Anthropic chat models** (up to 50% off,
  24h completion window). It does not cover embeddings. Nightly backfill is synchronous
  calls; at $0.02 per 1M tokens that is roughly $5 per GB of text, so it does not need
  batching.

Pick BGE-M3 or Qwen3 0.6B when vectors arrive (Phase 4, after FTS proves insufficient).
Both are also runnable locally for the work channel.

## Human-time signal

Decided: claim/release on approval items. Recorded in `DESIGN.md`.

## Pending: work-side checks

Run on the work laptop against a live Coder workspace.

```sh
# 1. Docker socket / DinD features in the work repos' devcontainers
grep -rn -i 'docker-in-docker\|docker-outside\|docker.sock\|privileged\|capAdd' \
  */.devcontainer/devcontainer.json */.devcontainer.json

# 2. Inner devcontainer privilege and caps (from inside the running devcontainer)
grep CapEff /proc/self/status          # want 00000000a80425fb or fewer, not 1fffffffffff
ls -la /var/run/docker.sock 2>&1       # want: No such file

# 3. Coder autostop policy
coder templates list; coder schedule show <workspace>   # or the template's autostop setting in the UI

# 4. Python and uv in the OUTER pod (where the node and consulta sidecar run)
python3 --version; uv --version; curl -sI https://astral.sh/uv/install.sh | head -1
```

Exit criteria not yet met from here: (1) and (2) decide whether the work pod's boundary
holds as-is; (3) decides whether background work at work is possible at all; (4) is a
bootstrap detail.
