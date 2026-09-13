# Recovery

The node is developed by agents running inside it, so a bad build must not lock
the operator out of the tool needed to fix it. This is the documented way out
and back: how to keep working on a tracon-managed workspace when the node is
down or a session is stuck, and how to rebuild the node itself from a backup.

**This is a recovery route, not a second supported harness.** Everything below
runs the harness as the operator, outside the boundary, with the operator's own
credentials — which is exactly what tracon exists to avoid doing routinely. It
is written down because a recovery path that has never been described is not a
recovery path. Use it to get unstuck, then come back through a session.

The shape is always the same three steps:

1. Get the work out of tracon.
2. Run the harness directly against it.
3. Bring the result back as a new session, and let the gate apply to it.

## 1. Get the work out

**With the node running.** A workspace outlives its session, so nothing has to
be rescued from a live harness:

| | |
|---|---|
| the files | Workspaces → download, or `GET /api/workspaces/{id}/download` — a zip, and `{id}` may name the session instead |
| the documents | `tracon doc export <dir>` — plain `<slug>.md`, archived ones under `archive/` |
| an HTML document | Documents → download (the original file, or a ZIP of the bundle) |
| a candidate and its evidence | a session's export: a signed package, readable offline (below) |

**With the node not running.** The workspace is a runtime-owned volume rather
than a directory in the operator's home, so reach it through the runtime:

```sh
podman volume export tracon-workspace-<workspace-id> -o work.tar
```

The workspace id is the one the session row records, and `node.db` still has it
with nothing running. On the Kubernetes backend the volume is the session's
claim; on the local backend used by tests it is
`local-runtime/tracon-workspace-<workspace-id>` under the state directory. The `[docs] export_dir` mirror, if it is configured, is
already on disk and needs nothing running at all.

Often the fastest answer is none of these: an execute session that submitted
anything has a branch on the forge, and that branch is a normal clone away.

## 2. Run the harness directly

Both harnesses are ordinary programs. Run one in the checkout or the unpacked
workspace, with the operator's own configuration and credentials:

```sh
cd <the work>
claude                     # Claude Code
opencode                   # OpenCode's TUI; `opencode serve` for its own API
```

Neither reads anything of tracon's. OpenCode uses the operator's
`opencode auth login` credentials and the config under `$XDG_CONFIG_HOME/opencode/`,
not the node's generated `opencode.json`; Claude Code uses its own login. That
is the point — nothing in tracon is required for this step, which is what makes
it a recovery route rather than another feature to keep working.

**What does not apply while you work this way:**

- **The boundary.** No container, no network policy, no runtime-owned volume.
  The harness has the operator's filesystem and the operator's network.
- **The gateway.** Model calls go straight to the provider on the operator's
  key. No egress allowlist, no per-session mediation, no channel ceiling, and
  no tokens counted against a budget.
- **Policy.** Nothing is refused with a reason and nothing reaches the queue as
  a card. Every decision is the operator's, in the moment, unrecorded.
- **The broker.** The harness holds real credentials, because they are the
  operator's own and they are on the same UID. Anything it can read, it can
  read.
- **The record.** No session log, no provenance for the commits, no metrics.

If the node is up and the problem is a stuck *session* rather than a broken
node, prefer the middle road: `[external]` gives a harness you start yourself
the node's tools — the forge, the tracker, the ledger, documents, memory, and
`submit_review` against your own worktree — without the operator ever handing
it a credential. It gives up the boundary and nothing else, and the work still
lands through review. `tracon external show` prints the line to register it.
`docs/reference/external-harness-notes.md` says what that mode costs.

## 3. Bring it back

- **Documents.** `tracon doc import <dir>` reads back exactly what
  `tracon doc export` wrote, by filename: no ids, no manifest, nothing tying
  the files to the node that wrote them.
- **Code.** Workspaces → import the files; the node copies them into a fresh
  runtime-owned volume (never a bind mount, Git metadata refused) and a new
  session starts on that workspace. A package exported from a session imports
  the same way and keeps the candidate's identity, its evidence, and the
  documents and memories that went with it.
- **The change itself.** Submit it from a session, so the checks run in the
  boundary and a review session reads the diff. A diff made outside has no
  check evidence and no review history; running it through the gate is how it
  gets some.

**What is permanently lost for the work done outside**, and cannot be
back-filled by the import:

- the audit trail — which model, which prompts, which tool calls, which
  approvals — for everything done in the direct harness;
- budget and usage accounting for those tokens; they were never the node's to
  count;
- review evidence for changes made outside: the container checks did not run,
  and the revisions the operator did not submit have no decisions on them;
- provenance for any commit not published through the node.

This is acceptable precisely because it is bounded and visible. The gate is on
the *publish*, not on the editor: a change that came in this way still cannot
reach the forge without a review session and an authorized publish, so the
worst case is a change with a thin history, not an unreviewed merge. What would
not be acceptable is making this the ordinary path — at which point the
guarantees are gone and nothing says so.

## Recovering the node itself

Everything a node is lives in two directories. On Linux that is
`~/.config/tracon/` for `node.toml` and `~/.local/state/tracon/` for the rest;
on macOS both are `~/Library/Application Support/tracon/`.

| | | |
|---|---|---|
| `node.toml` | the configuration | back it up; hand-writable |
| `node-identity.seed` | the node's identity, 32 bytes of hex | **back it up first** |
| `credentials.sealed` | the broker | sealed under a key derived from the seed — worthless without it |
| `node.db` | sessions, corpus, ledger, channel keys, candidates and evidence | back it up |
| `policy.toml`, `policy.toml.sig`, `policy-key.pub` | the signed bundle the node reads at startup | back it up |
| `policy-key` | the signing key, if this machine signs bundles | back it up, separately |
| `providers/` | harness provider logins | re-doable by logging in again |
| `local-runtime/`, `workspaces/`, `harness-state/`, `git-home/` | runtime volumes, snapshots, caches | derived; large; skip |
| the vector index, inside `node.db` | semantic search | derived; `tracon doc reindex` |

**A fresh node needs, at minimum, `node-identity.seed` and `node.db`.** The
seed is the node's name on the mesh and the key its credentials are sealed
under; the database holds the channel keys without which a meshed node refuses
to start a session on a channel. Restore both, put `node.toml` back, and
`tracon service install` brings the node up as itself.

Without the seed, recovery is re-enrollment rather than restore: generate a new
identity, `tracon enroll` against the hub with a fresh invitation, and have the
channels shared to the new node id. Credentials must be imported again —
`credentials.sealed` cannot be opened by a different identity, by design. The
corpus comes back on its own once the channel keys are there: every node keeps
a replica, so documents, memories, and the ledger re-replicate from the hub.
Sessions, candidates, and evidence do not — they are node-local, and `node.db`
is the only copy.

The hub has its own story: `TRACON_HUB_SNAPSHOT_*` writes encrypted snapshots
to object storage and `tracon-hub restore` reads one back with
`TRACON_HUB_RESTORE_SEED`.

## Reading an archive with nothing running

A session's export is a plain JSON package, and it stays readable without a
node, a database, or a harness:

```sh
tracon session show package.json            # a summary, and whether it verifies
tracon session show package.json --jsonl    # one tagged JSON record per line
```

The JSONL stream is the whole package: a `package` manifest line, the
`candidate` and its `evidence`, the handoff `note`, one line per carried
`document` and `memory`, and one per `file` with its size and SHA-256. Every
line names its own `kind`, so `grep` and `jq` are sufficient tools. A package
that does not verify still renders — it says so on the manifest line rather
than refusing to open, because an archive that will not open is not an archive.

## Rebuilding the vectors

The vector index is derived data. It is node-local, it never replicates, and it
is reconstructible from the documents and memories the node holds, so it is
never part of a backup:

```sh
tracon doc reindex
```

That drops the index and builds it again from the corpus, reporting what it
embedded. The background sweep only fills in what is missing or stale per
record, which cannot repair an index that is present but wrong — a half-written
file, a changed chunker, a model swapped underneath the same `dim`. This can.
A node with `[embed]` off has no index and says so; retrieval there is FTS-only
and complete without it.
