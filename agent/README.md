# tracon-agent

The harness tracon owns, for models that are not Claude: local llama.cpp now, the Codex
subscription later. It is a separate binary that runs inside the boundary, like
`claude`. The node supervises it and does no reasoning of its own. The plan is
`plan-tracon-native-harness` on the personal channel.

**Status: Phase 0, standalone only.** It runs without a node or a permission gate, and
refuses to start without `--standalone`. Run it only in a directory you would let any
command loose in.

## Running it

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

Under the node these become `session/update` and per-request `usage`.

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
