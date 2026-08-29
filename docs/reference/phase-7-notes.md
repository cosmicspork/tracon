# Phase 7 notes

Written 2026-08-29, during the work. What is here is what could not be looked up:
protocol shapes that are in no published document, and two build failures whose
cause is not obvious from the error.

## Claude Code's control protocol

Version 2.1.247. `claude --help` documents neither the control protocol nor
`--permission-prompt-tool`, and reading the published docs leads to the wrong
conclusion — that an external permission broker needs the TypeScript Agent SDK.
It does not: the SDK spawns this same binary and speaks stream-json to it.

The evidence is in the binary. `strings` on it finds `can_use_tool` (30),
`control_request` / `control_response` (67 / 64) and `permissionPromptTool`
(24), along with the schema documentation the SDK is generated from — including
this, which settles it:

> With a permission prompt surface (stdio/SDK canUseTool), the 'ask' path
> surfaces via a `can_use_tool` control_request.

Several flags absent from `--help` are still accepted, `--permission-prompt-tool`
among them. Probing for them is one command each:

```sh
claude --append-system-prompt-file /tmp/x --version   # accepted
claude --permission-prompt-tool stdio --version       # accepted
```

### The frames

`system/init` arrives **before any model call**, which is what makes it safe to
block on during launch, and cheap to use for a model probe. Confirmed live:

```json
{"type":"system","subtype":"init","session_id":"…","claude_code_version":"2.1.247",
 "model":"…","permissionMode":"default","cwd":"…","apiKeySource":"ANTHROPIC_API_KEY",
 "tools":[…],"mcp_servers":[{"name":"tracon","status":"connected"}],
 "capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1"]}
```

Two things follow from it. `claude_code_version` means the pin can be checked a
second time from the harness's own report, the way the ACP adapter checks
`initialize.agentInfo.version` — this layer breaks silently, so it fails closed.
And `apiKeySource: ANTHROPIC_API_KEY` confirms the existing gateway wiring is
already exactly right: `harness_wiring` emits `ANTHROPIC_BASE_URL` and
`ANTHROPIC_API_KEY` for anthropic-shaped providers, and the gateway needed no
change at all.

A permission ask:

```json
{"type":"control_request","request_id":"req_1",
 "request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"toolu_1",
            "input":{"command":"git status"}}}
```

and its answer, correlated by `request_id`:

```json
{"type":"control_response",
 "response":{"subtype":"success","request_id":"req_1",
             "response":{"behavior":"allow"}}}
```

The response schema, extracted verbatim from the binary:

```
{ behavior: "allow", updatedInput?, updatedPermissions?, toolUseID? }
| { behavior: "deny", message, interrupt?, toolUseID? }
```

`control_cancel_request` exists for an ask whose turn was interrupted; either
side may send it for a request it originated, and there is no reply to the
cancel itself.

`--input-format stream-json` keeps stdin **open**: `--replay-user-messages` is
documented as re-emitting "user messages from stdin", and a session takes many
turns on one process. A second prompt is another `{"type":"user",…}` line.

`result` ends a turn and carries `total_cost_usd` and token counts, but **no
total**, so the budget has to sum the parts or it charges zero.

### Reproducing any of this

An invalid base URL gives the init and result frames without spending anything:

```sh
CLAUDE_CONFIG_DIR=$PWD/state ANTHROPIC_API_KEY=x ANTHROPIC_BASE_URL=http://127.0.0.1:1 \
  claude --print --input-format stream-json --output-format stream-json --verbose \
    --session-id "$(uuidgen)" --permission-mode default --strict-mcp-config \
  < <(echo '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}')
```

### Traps

- **`--permission-mode` must stay `default`.** `dontAsk` and `bypassPermissions`
  decide tool use inside the harness, so the node never sees the request it
  exists to broker and the operator's queue simply stays empty. There is a test
  asserting the argv, because this is not a failure anything would report.
- **`LocalRunner` spawns with `kill_on_drop(true)`.** Dropping the `Spawned.done`
  future kills the harness the instant it starts, which looks exactly like a
  harness that said nothing. Hold it; it is also where the exit code comes from.
- **The node picks the session id** and passes `--session-id`, rather than
  discovering it, so the row it already wrote and the harness's own id are the
  same string even if the handshake fails.

## sqlite-vec on static musl

`ARCHITECTURE.md` names `sqlite-vec`, and the node ships as a statically linked
musl binary for two architectures. It works, but not out of the box.

**1. `u_int8_t` does not exist on musl.** `sqlite-vec.c` has

```c
#ifndef _WIN32
#ifndef __EMSCRIPTEN__
#ifndef __COSMOPOLITAN__
#ifndef __wasi__
typedef u_int8_t uint8_t;
```

which is a BSD/glibc spelling musl does not define, so the extension fails to
compile for *both* release targets. Defining the three names as their standard
equivalents makes those typedefs self-referential, which C11 permits. It lives
in `.cargo/config.toml` rather than in CI so a local `cargo zigbuild` and the
release workflow cannot disagree:

```toml
[env]
CFLAGS_x86_64_unknown_linux_musl = "-Du_int8_t=uint8_t -Du_int16_t=uint16_t -Du_int64_t=uint64_t"
```

Remove it once sqlite-vec guards that block on `__GLIBC__`.

**2. `c_char` is signed on x86_64 and unsigned on aarch64.** The
`sqlite3_auto_extension` callback takes `*mut *mut c_char`; spelling it `i8`
compiles on one release target and not the other. Only the cross-build catches
it, so build both before believing it works:

```sh
cargo zigbuild --release --bin tracon --target aarch64-unknown-linux-musl
file target/aarch64-unknown-linux-musl/release/tracon | grep "statically linked"
```

**3. `vec0` has no upsert, and SQLite reuses rowids.** Deleting a metadata row
and inserting its replacement hands back the *same* rowid, so an orphaned vector
at that rowid is not a stale row the join hides — it is a primary-key collision
on the next write. Clear the vector by rowid before the metadata row goes.

## Running an embedding endpoint

`[embed]` takes an OpenAI-shaped `/v1/embeddings` service. For a work channel
that means one on the same machine:

```sh
llama-server --embedding -m ~/models/bge-m3.gguf --host 127.0.0.1 --port 8080
```

```toml
[embed]
enabled = true
base_url = "http://127.0.0.1:8080"
model = "bge-m3"
dim = 1024
```

`dim` must match the model. A mismatch is refused at the first call rather than
written, and changing it rebuilds the index from empty, because vectors from two
models are not comparable.

With `provider` set instead of `base_url`, the call goes through the node's own
model gateway, so the channel's provider binding and its daily ceiling apply.
That path needed the gateway's node-side token widened from GET-only to permit
POST — on an embeddings path and nothing else, since that token carries no
channel and anything it reaches is outside the per-channel bindings.

**Nothing here has been run against a real embedding model.** The live check
used a stub that scores a handful of topic words, which is enough to prove the
wiring — backfill, bus-driven indexing, ranking, the degraded path — but says
nothing about retrieval quality. Choosing between BGE-M3 and Qwen3 Embedding
0.6B is still open.

## The live check, for repeating it

A scratch node, isolated by XDG paths. `XDG_RUNTIME_DIR` must be short: the
harness socket path hits the ~108-character Unix socket limit otherwise, which
is what the `bind …/harness.sock` error means.

```sh
export XDG_CONFIG_HOME=$SCRATCH/config XDG_STATE_HOME=$SCRATCH/state \
       XDG_RUNTIME_DIR=/tmp/tn-run TRACON_URL=http://127.0.0.1:7455
tracon serve --listen 127.0.0.1:7455
tracon channel create personal
tracon doc import ./docs --channel personal
curl -s "$TRACON_URL/api/docs?channel=personal&q=how%20do%20I%20verify%20my%20work"
```

The query shares no words with the document that answers it, which is the point.
With the endpoint stopped, the same query returns `"text_only": true` and
whatever FTS can find.
