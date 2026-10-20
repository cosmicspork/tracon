# tracon-agent

The harness tracon owns, for models that are not Claude: local llama.cpp now, the Codex
subscription later. It is a separate binary that runs inside the boundary, like
`claude`. The node supervises it and does no reasoning of its own. The plan is
`plan-tracon-native-harness` on the personal channel.

**Status: Phase 1, experimental.** It has two modes:

- **Protocol mode** (the default). The node drives it over stdin and stdout and answers a
  permission request for every tool call. The node adapter is still to come.
- **Standalone** (`--standalone`). It runs one task with no node and no permission gate.
  Run it only in a directory you would let any command loose in.

## Under the node: protocol mode

stdin and stdout carry newline-delimited JSON-RPC 2.0, using the vocabulary in the
`acp` crate. That crate is shared with the node, which used it to drive omp.

| Method | Direction | Meaning |
|---|---|---|
| `initialize` | node → agent | Protocol version (1) and agent info. |
| `session/new` | node → agent | The session's `cwd`, and the `mcpServers` whose tools join the six built-in ones. |
| `session/prompt` | node → agent | Runs a turn. Answers with `stopReason` (`end_turn`, `max_turn_requests` or `cancelled`) and the turn's `usage`. |
| `session/cancel` | node → agent (notification) | Ends the running turn. |
| `session/close` | node → agent | Ends the session. |
| `session/update` | agent → node (notification) | Message and thought chunks, tool calls and their updates, and context usage. Each carries a monotonic `seq` and is mirrored to `--events`. |
| `session/request_permission` | agent → node | Sent before every tool call. Only `allow_once` runs it; anything else is a refusal the model is told about. |

**Usage** follows the gateway's meaning. `inputTokens` is the prompt not read from a cache,
`cachedReadTokens` is what was, and the node can reconcile the two counts per turn.

**Credentials and the model** come from the command line, never from the session:
`--base-url`, `--model`, `--api-key-env` (the name of the variable holding the node's
per-session key), `--context-window` and `--system-prompt-file`.

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
- `--parallel-tools`.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | The model answered without a tool call. |
| 1 | Error. |
| 3 | The turn limit was reached. |
| 4 | The context window was exceeded. Nothing is truncated; compaction comes in Phase 2. |

The endpoint must speak OpenAI Chat Completions with streamed tool calls. For llama.cpp
that means `--jinja`, with the server's built-in `--tools` left off.

## Events

`--events` writes one JSON object per line. Without it they go to stderr. The events are:

- `start`
- `request` and `response`. A response carries:
  - usage and `request_id`;
  - wall time, time to first token and tokens/s;
  - llama.cpp's own `timings`.
- `tool_call` and `tool_result`
- `error`
- `end`, with the outcome and totals.

In protocol mode the file also records the session (`session`, with which instruction
files were read) and each `update` with its `seq`.

## Tools

The tools are `read`, `edit`, `write`, `glob`, `grep` and `shell`, and nothing else.
Every path is resolved inside `--cwd`: escapes with `..`, absolute paths and symlinks
are refused. `glob` and `grep` respect `.gitignore`. `shell` runs `sh -c` with piped
output, a timeout, and its process group killed on timeout. It is not a PTY yet.

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
