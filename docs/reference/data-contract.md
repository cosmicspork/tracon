# Data contract, version 1

Everything a node holds, as one file that reads with nothing running. Version 1 is
export only: nothing reads an export back into a node. Import is a later, separate
contract.

```sh
tracon data export -o node.jsonl                 # from the running node
tracon data show node.jsonl                      # counts per kind, and whether it verifies
tracon data show node.jsonl --jsonl --kind memory
tracon data show node.jsonl --documents ./docs   # the documents, as document export writes them
```

**Settings → System → Export everything** downloads the same file. The route is
`GET /api/maintenance/export`, open to the operator at the node and to an
explicitly authenticated administrator elsewhere, because the file carries every
transcript.

The contract builds on three readers that already exist and does not replace them:
the [session package](../RECOVERY.md#reading-an-archive-with-nothing-running) (a
signed candidate JSON, read by `tracon session show`), the signed candidate it
carries, and [document export](cli.md) (`tracon doc export`, `[docs] export_dir`).
Each package the node holds is inside the export, verbatim, as a `transfer` row's
`package_json`, and `tracon data show` verifies each one as `tracon session show`
does. `--documents` writes the export's Markdown documents with the same code
document export uses.

## The file

UTF-8 JSONL: one JSON object per line, each ending `\n`. Three shapes of line, in
this order:

1. **Header** (first line): `kind: "export"`.
2. **Records**, zero or more.
3. **Trailer** (last line): `kind: "end"`.

A file without a trailer was cut short; a reader says so and still reads what is
there. Anything after the trailer is not covered by it and is ignored.

### Header

```json
{"kind":"export","contract":"tracon/data/v1","contract_version":1,
 "node":"<hex ed25519 public key>","tracon_version":"0.59.0","created_ms":1763900000000,
 "store_schema":88,"sync_schema":10,"warning":"…",
 "kinds":[{"kind":"session","tables":["session","turn_usage","…"]},"…"],
 "omitted_tables":[{"table":"kv","reason":"…"},"…"],
 "omitted_columns":[{"table":"channel","column":"keyring","reason":"…"},"…"]}
```

- `contract` and `contract_version` name this document. A reader refuses a
  contract it does not know.
- `node` is the exporting node's id: the hex of its Ed25519 public key, as the
  [mesh contract](../../spec/README.md) defines it.
- `store_schema` is the node database's `user_version` and `sync_schema` the
  replicated tables' step. Together they say which columns a row can have.
- `kinds`, `omitted_tables` and `omitted_columns` repeat the tables below as this
  export applied them, so an export describes itself.

### Records

```json
{"kind":"memory","table":"memory","row":{"id":"…","channel":"personal","body":"…","created_ms":1763900000000}}
{"kind":"workspace","directory":"workspaces","row":{"name":"ws-…","files":412,"bytes":1873221}}
```

- `kind` is one of the contract's kinds below.
- `table` names the node table the row comes from; `row` holds the row's columns by
  name, less the omitted ones. A column added to a table in a later release appears
  in `row` without a change of contract version, so a reader ignores columns it
  does not know.
- A workspace directory record has `directory` in place of `table`.

Records come kind by kind in the order below, each table's rows in insertion order
(SQLite `rowid`; a table without one in no stated order).

### Trailer

```json
{"kind":"end","records":1234,"sha256":"<hex>","node":"<node id>","signature":"<hex>"}
```

- `records` counts the record lines.
- `sha256` is SHA-256 over every byte before the trailer: the header and record
  lines, each with its `\n`.
- `signature` is Ed25519, by `node`'s key, over
  `"tracon/data-export/v1" ‖ 0x00 ‖ sha256` (the 32 raw digest bytes).

## Identifiers

Every identifier is the node's own, carried as stored: session ids, work item ids,
candidate ids (`<head sha>:<channel>`), document and memory ids, transfer ids (the
package's SHA-256), node ids (hex Ed25519 keys) and channel names. A row refers to
another by the same value, so `session.work_item_id` matches a `work_item` row's
`id`, an `event` row's `session_id` a `session` row's `id`, and so on. Names the
operator gave records are `entity_name` rows; a record with none reads by the name
generated from its id.

## Encodings

| SQLite value | In `row` |
|---|---|
| NULL | `null` |
| INTEGER | a JSON number (64-bit: a reader in JavaScript loses precision above 2^53) |
| REAL | a JSON number, or `null` when not finite |
| TEXT | a JSON string. A column holding JSON (`*_json`, `capture_json`, `package_json`) stays a string: parse it yourself |
| BLOB | `{"base64": "<standard base64>"}` |

Times are milliseconds since the Unix epoch (`*_ms`), as the node stores them.
Booleans are integers, 0 or 1.

## Kinds

| Kind | What | Tables |
|---|---|---|
| `session` | sessions and what they spent, the launch manifests they started with, channels, nodes, record names | `session`, `turn_usage`, `model_usage`, `harness_request_usage`, `sandbox_usage`, `provider_limit_snapshot`, `exhaustion`, `operator_notification`, `launch_manifest`, `manifest_item`, `channel`, `node`, `node_channel`, `entity_name` |
| `event` | session logs: every transcript event, the node's and external harnesses' | `event`, `external_event` |
| `work_item` | the ledger | `work_item`, `project`, `pipeline_follow` |
| `decision` | what was asked and answered: approvals, permission requests, questions, review decisions, authority grants and actions, recipe offers | `approval`, `permission_request`, `operator_question`, `review_decision`, `authority_grant`, `authority_action`, `repo_recipe_offer` |
| `draft` | what was written and not yet sent: review drafts, queued prompts, issue drafts | `review_draft`, `queued_prompt`, `operator_issue` |
| `document` | documents, HTML bundles included | `document`, `document_bundle_file`, `document_bundle_chunk` |
| `memory` | memories and their promotions | `memory`, `promotion` |
| `workspace` | where each workspace came from, harness state, repository images, catalogue services' persistent volumes, and each workspace directory by name, file count and size (not its files) | `workspace_origin`, `opencode_session`, `opencode_object`, `opencode_intent`, `opencode_state`, `opencode_part_claim`, `repo_image`, `service_volume`; directories `workspaces/`, `workspace-staging/` |
| `evidence` | candidates and their files, checks, demonstrations, reviews with their revisions, dependencies and threads, publications, QA runs, prototypes | `candidate`, `candidate_file`, `check_run`, `demonstration`, `review`, `review_revision`, `review_revision_trigger`, `review_session_revision`, `review_dependency`, `review_thread`, `review_thread_comment`, `publication`, `shown_work`, `qa_deployment`, `qa_browser_run`, `qa_asset`, `prototype` |
| `provenance` | where things came from: session packages (signed, verbatim) and their history, configuration changes, installed policy, the replication log | `transfer`, `transfer_event`, `transfer_import`, `config_audit`, `policy_installation`, `policy_rollout`, `policy_rollout_target`, `change_log` |

A table the node does not have (an older schema) is simply absent. The node's
build refuses a table that is in neither this list nor the omissions, so a new
table is always placed on purpose.

## Omission rules

1. **Credentials and private keys are never written.** No table holds a private key
   (the node's seed is a file, `node-identity.seed`, and is never read by the
   export), and the tables and columns that hold credentials, a credential's hash, or
   wrapped keys are left out:

   | Left out | Why |
   |---|---|
   | `kv` | node settings, among them the hash of the operator token |
   | `auth_session`, `ui_grant`, `device_approval` | signed-in clients and their credentials' hashes |
   | `webauthn_credential`, `webauthn_challenge` | passkeys |
   | `push_subscription`, `push_announced` | push endpoints and their keys |
   | `credential_revoke`, `credential_revoke_target`, `repo_credential_hold` | which credentials exist and who holds them |
   | `channel.keyring` | the channel's keys, wrapped to this node |
   | `qa_deployment.test_credential` | a credential for the deployment under test |

   The broker's credential store (`credentials/`, sealed) is a file, not a table,
   and is never read.
2. **Transport and bookkeeping are left out**: `mesh_outbox`, `mesh_held`,
   `mesh_cursor`, `mesh_seen`, `mesh_rollup_seq`, `mesh_admin_statement`,
   `operator_notification_attempt`, `operator_notification_rate`, `hlc`,
   `sync_schema` (in the header instead).
3. **Derived data is left out**: `embedding` and its vector tables, rebuilt from
   documents and memories with `tracon doc reindex`, and the full-text indexes
   `document_fts` and `memory_fts`, which SQLite keeps from those rows.
4. **Plugins are left out**: `plugin` rows and plugin directories are programs and
   each plugin's own data, not the node's records.
5. **Deletes are left out**: a row whose `deleted` column is not 0 is a tombstone, a
   delete the node remembers, and is not exported.
6. **Workspace files are left out**: a workspace is named and weighed, not copied.
   A session package carries a candidate's files when they are wanted.

### Transcripts are not redacted

The export carries every prompt, transcript event, tool output, document and memory
as it was written. Anything that passed through a session — a key pasted into a
prompt, a token a command printed, private data read from a repository — can be in
it. The header's `warning`, `tracon data export`, `tracon data show` and the
interface each say so. Keep the file as you would keep `node.db`.

## Verifying grants nothing

A verified trailer says two things: the file is byte for byte what the node wrote,
and which node wrote it. It does not make that node, the file, or anything in it
trusted, enrolled, authorised or imported. The same holds for each session package
inside: `tracon data show` reports how many verify, and a verified package is still
only read. Reading an export — verified or not — opens no session, writes no store,
admits no peer and changes nothing on any node. A file that does not verify still
reads, and says so.

## Versioning

`contract_version` moves only for a change a version 1 reader would misread: a
renamed or removed kind, a changed encoding, a changed line shape. New columns, new
tables under an existing kind and new omissions are additive and keep version 1; a
reader ignores what it does not know, and counts a record of an unknown kind
rather than failing on it.
