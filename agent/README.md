# tracon-agent

The harness tracon owns, for models that are not Claude: local llama.cpp over Chat
Completions, and the Codex subscription over the Responses API. It is a separate binary that runs inside the boundary, like
`claude`. The node supervises it and does no reasoning of its own. The plan is
`plan-tracon-native-harness` on the personal channel.

**Status: Phase 1, experimental.** It has two modes:

- **Protocol mode** (the default). The node drives it over stdin and stdout and answers a
  permission request for every tool call. The node's adapter is `node/src/adapter/native.rs`.
- **Standalone** (`--standalone`). It runs one task with no node and no permission gate.
  Run it only in a directory you would let any command loose in.

## Under the node: protocol mode

stdin and stdout carry newline-delimited JSON-RPC 2.0, using the vocabulary in the
`acp` crate. That crate is shared with the node, which used it to drive omp.

| Method | Direction | Meaning |
|---|---|---|
| `initialize` | node → agent | Protocol version (1), agent info, and `agentCapabilities.loadSession` when sessions are journaled. |
| `session/new` | node → agent | The session's `cwd`, and the `mcpServers` whose tools join the six built-in ones. Answers with a new session id (a UUID). |
| `session/load` | node → agent | Carries on a journaled session in this process: the same `sessionId`, `cwd` and `mcpServers`. |
| `session/prompt` | node → agent | Runs a turn. Answers with `stopReason` (`end_turn`, `max_turn_requests` or `cancelled`) and the turn's `usage`. |
| `session/cancel` | node → agent (notification) | Ends the running turn. |
| `session/close` | node → agent | Ends the session. |
| `session/update` | agent → node (notification) | Message and thought chunks, tool calls and their updates, context usage with each request's own usage, and retry notices. Each carries a monotonic `seq`, continued across a load, and is mirrored to `--events`. |
| `session/request_permission` | agent → node | Sent before every tool call. Only `allow_once` runs it; anything else is a refusal the model is told about. |

**Usage** follows the gateway's meaning. `inputTokens` is the prompt not read from a cache,
`cachedReadTokens` is what was, and the node can reconcile the two counts per turn. Each
`usage_update` also carries `request`: that one upstream request's tokens and `requestId`
(the response's `id`), for matching against the gateway request by request.

**A turn the agent stops itself** answers with a JSON-RPC error that says why:
- the model wrote a tool's result itself (`<tool_response>` and the like in its text), so
  nothing acts on an invented result;
- the model made two malformed responses in a row. A malformed call (no tool name, an
  unknown tool, arguments that are not a JSON object, or a call cut off by the output
  limit) is never asked about: it is shown to the node as failed, and the model gets a
  result saying what was wrong. A call written into the text instead of made counts too.

**Retries.** A model request that fails with 408, 429, 500, 502, 503, 504 or 529, a lost
connection, or a stream that breaks off is made again after 1, 2, 4 and 8 seconds, or after
the upstream's `Retry-After` when that is longer (up to 60 seconds; longer, and the turn
ends). Each retry is sent as a `session/update` with `subtype: "api_retry"`. Once any
answer text has reached the node the request is never made again: the text is kept in the
conversation, so it matches what the operator read, and the turn ends with the error.

**The journal** is what makes a session outlive its process. With `--journal-dir` (by
default `sessions/` beside `--events`), each session appends `<id>.jsonl`, written before
each step takes effect: the prompt, the model's response with its tool calls before any of
them runs, each call's start once it is allowed, and its full result. `session/load`
rebuilds the conversation from it. A call the last process died around gets a result
here:
- one that never started: "not run: the harness restarted before this call ran";
- one that started and has no result: "outcome unknown", with the model told to check its
  effects before running it again.

Either is sent to the node as the call's failed update, numbered after the last `seq` the
dead process sent.

**Compaction** keeps a session inside `--context-window`. Before a request estimated to
come within 15 % (at least 16k tokens) of it, old tool outputs beyond the newest 40k
tokens are replaced by `[Output truncated - N tokens]` if that saves 20k; a read or edit
of a file that a later edit or write changed, or a read a later read of the same lines
repeated, goes whatever its age. If the
conversation still does not fit, the same model summarises everything before the last
20k tokens (never parting a call from its result), and the summary, followed by the
harness's own record of the tool calls made and how each ended, and the operator's
messages word for word, replaces it. The record and the operator's words are copied, not
summarised, so a summariser that guesses cannot change what the agent believes ran or was
asked for. For a small window those sizes scale
down. A request the upstream refuses as too long (`exceed_context_size_error`) is
compacted and made once more, with or without a declared window; a second refusal ends
the turn. Each step is journaled (`pruned`, `compacted`) and reported to the node as a
`context_compacted` update.

**Credentials and the model** come from the command line, never from the session. The
node passes `--providers`, a file it stages per session listing each `openai`-shaped
provider's gateway URL, key variable and declared models, with `--model provider/model`.
The file wins over `--base-url`, `--api-key-file` and their `TRACON_AGENT_*` variables, so
a repository image cannot redirect a session. Without it, `--base-url`, `--model` and
`--api-key-env` (the name of the variable holding the key) say the same directly.
`--context-window` and `--system-prompt-file` complete it.

**The system prompt**, in order:
1. the built-in orientation;
2. the node's text;
3. the repository's `AGENTS.md` and `CLAUDE.md`, recorded in the events file as read.

Nothing but the protocol is written to stdout.

## Standalone

```sh
cargo build --release -p tracon-agent
target/release/tracon-agent --standalone \
  --base-url http://127.0.0.1:8080/v1 --model qwen3-8b \
  --api-key-file ~/.config/llama-server.key \
  --cwd /path/to/scratch/repo --events run.jsonl \
  --prompt "Add a mul(a, b) function to calc.py after sub."
```

The prompt can also come from stdin. Other flags:

- `--max-turns` (default 20).
- `--temperature` and `--min-p`. llama.cpp's `min_p` default of 0.05 may need 0.01.
- `--no-think`, which sends Qwen3's `enable_thinking=false`.
- `--parallel-tools true|false` (default `true`): whether several calls may come in one
  response.
- `--reasoning-effort` (`low`, `medium`, `high`, …), on the Responses wire.

- `--wire chat|responses` (default `chat`). With `--providers` the provider's shape
  decides: `openai` is Chat Completions, `openai-codex` is Responses.

Under the node, a model's `parallel_tools` and `reasoning_effort` in `node.toml` reach
the agent through the providers file; a flag given on the command line wins.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | The model answered without a tool call. |
| 1 | Error. |
| 3 | The turn limit was reached. |
| 4 | The context window was exceeded, and compacting the conversation did not bring it within. Nothing is truncated. |
| 5 | The agent stopped the turn itself (see above). |

On `chat` the endpoint must speak OpenAI Chat Completions with streamed tool calls. For
llama.cpp that means `--jinja`, with the server's built-in `--tools` left off.

On `responses` (the Codex backend) nothing is stored upstream (`store: false`), so each
request replays the conversation. The system prompt goes in `instructions`, which the
backend accepts as ours. Reasoning comes back as encrypted items, kept in the
conversation as `Opaque` blocks and sent again in place, without their ids. The session
id goes as `prompt_cache_key` and the `session_id` header, without which the backend's
prompt cache rarely hits. No `max_output_tokens` is sent; the backend refuses it. The agent's own tools are sent
`strict`, with optional fields made nullable, so the upstream holds the model to their
schemas; a node tool's schema is its server's and goes as it came.

## Events

`--events` writes one JSON object per line. Without it they go to stderr. The events are:

- `start`
- `request` and `response`. A response carries:
  - usage and `request_id`;
  - wall time, time to first token and tokens/s;
  - llama.cpp's own `timings`.
- `tool_call` and `tool_result`
- `retry`, with the attempt, the wait and the error
- `stopped`, with the reason
- `error`
- `end`, with the outcome and totals.

In protocol mode the file also records the session (`session`, with which instruction
files were read, and on a load how much was restored and which calls were left open) and
each `update` with its `session_id` and `seq`. The file is appended to, never truncated.

## Tools

The tools are `read`, `edit`, `write`, `glob`, `grep` and `shell`, plus `spawn` (see
Subagents), and nothing else.
Every path is resolved inside `--cwd`: escapes with `..`, absolute paths and symlinks
are refused. `glob` and `grep` respect `.gitignore`. `shell` runs `sh -c` with piped
output, a timeout, and its process group killed on timeout. It is not a PTY yet. While a
command runs, the last 2 KiB of what it has printed goes to the node once a second (a
running `tool_call_update` whose `rawOutput` is the tail), so the session shows it before
the result.

### Hashlines

`read` and `grep` show each line as `LINE:HASH|text`, for example `12:a3f9|def main():`.
`HASH` is the first four hex characters of the SHA-256 of the line. `edit` names lines by
these anchors:

```json
{"path": "calc.py", "edits": [
  {"op": "replace", "start": "5:05a6", "end": "6:dd3b", "text": "def sub(a, b):\n    return b - a"},
  {"op": "insert_after", "start": "10:e27d", "text": "print(mul(2, 3))"}
]}
```

The ops are `replace`, `delete`, `insert_after` and `insert_before`.

Every anchor is checked against the file before anything is written, and one mismatch
refuses the whole call. The refusal shows what the line is now. When a model paired one
line's number with another line's hash, it shows both lines rather than guessing between
them. A successful edit returns fresh anchors around the change.

## Subagents

`spawn` hands a task to a subagent: a child conversation in the same process, with the
same tools (not `spawn`: one level only), its own orientation, and the task as its first
message. It ends by calling `finish`, a tool whose parameters are the `result_schema`
the parent gave (default `{summary: string}`); a result that does not fit the schema is
refused and asked for again, and that object, as JSON, is the spawn call's result. A
child that answers in prose instead is reminded once, then stopped.

- **Parallel.** The spawn calls in one response are asked about in turn, then those
  allowed run at once, four at most, and their results come back in the order they were
  made. Any other call still runs one at a time. A model can only put several calls in
  one response when its `parallel_tools` is on, as it is unless turned off
  (`--parallel-tools false`, or `parallel_tools = false` on the model in `node.toml`);
  off, each spawn comes in a response of its own and the children run one after another.
- **Model.** `model` may name any model the node declares under the session's provider;
  by default a child uses the parent's. Nothing outside that provider can be named.
- **Asked like the parent.** Each of a child's calls goes to the node, which shows it
  under the spawn call (`parentToolCallId`), with an id of its own (`<spawn id>/<id>`)
  and a title that starts with the child's task. Spawning itself is kind `think`, which
  the shipped policy allows.
- **Paid by the parent.** Children use the session's gateway key, so the session's
  budget covers them; each of their requests is reported by id like the parent's, and
  their usage is part of the turn's.
- **Not journaled.** A child lives only as long as its spawn call; if the process dies
  during one, the call comes back "outcome unknown" like any other, and the parent may
  spawn again.

## Turning it on for a node

1. Add `"native"` to `[harness] experimental` in `node.toml`.
2. Give an `openai`-shaped provider some declared models. For the llama.cpp server on this
   machine, for example:

   ```toml
   [providers.local]
   credential = "local"                # an api_key credential holding the server's key
   upstream = "http://127.0.0.1:8080"  # the node dials it, so host loopback works
   shape = "openai"
   models = [{ id = "qwen3-8b", context = 32768 }]
   ```

   Also add the upstream host to `[gateway] allow_hosts`.
3. Run `tracon setup`. It builds `tracon-harness-native` from this version's release asset.
   For a version with no release yet, build a binary with `just native-agent`, set
   `[harness] native_binary` to the path it prints, and run `tracon setup` again.

Models on that provider then offer a choice of harness, OpenCode or `native`. A model whose
credential runs only here (none today) would start here directly.
