# Building Tracon plugins

This guide is for people writing a plugin: the manifest, the release, the process the
node starts, and the calls it makes. Operators installing one want
[the plugins guide](../guides/plugins.md#plugins) instead. Everything here is contract v1, and
the code on `main` is the authority: `node/src/plugin/` (manifest, install, supervisor,
caller context), `node/src/mcp/upstream.rs` (the transport) and the plugin paths in
`node/src/mcp/mod.rs`. Where this guide and the code disagree, the code wins and this
guide is the bug.

Two real plugins are the worked examples throughout:
[`sql-review`](https://github.com/cosmicspork/sql-review) and
[`jira-review`](https://github.com/cosmicspork/jira-review). Both are single Rust
binaries; nothing in the contract requires Rust.

## What a plugin is, and when to write one

A plugin is an executable, optionally with a static UI, released on GitHub in a fixed
shape. The node installs a build only when the operator allows its install card, pins
it by digest, runs it as one process per plugin and channel, and brokers its tools to
sessions and external harnesses under policy. Each call carries a signed caller
context, and the credentials the call needs are lent in that context, per call.

An `[[mcp_server]]` entry is the other way to add tools. Choose between them like this:

| | `[[mcp_server]]` | plugin |
|---|---|---|
| Where it runs | Anywhere, reached over streamable HTTP | On the node's machine, started and supervised by the node |
| How it is added | A config entry | An install card naming a release by digest |
| Credentials | One credential, sent as a header (`TOKEN`) on every request | Any declared credentials, lent per call in a signed context; some only on approval |
| Who is calling | Unknown to the server | Signed: operator or agent, session, harness, channel, approval |
| Approval cards | Generic | Generic, plus the plugin's own preview, staleness enforced at dispatch, and formats it renders |

Write an `[[mcp_server]]` when the tools already live behind a hosted MCP endpoint, or
when nothing about the caller or the approval matters to the server. Write a plugin when
the server holds credentials it should not keep, when a write should be applied only
against what the operator saw, or when you want the node to supervise the process.

### Three modes, one binary

Every plugin should be useful without Tracon. sql-review and jira-review are each one
binary with three modes:

- **CLI**, for people: `sql-review --sql 'SELECT 1'`, `jira-review issue WRK-1`.
- **`mcp`**, a stdio MCP server for any agent client. Writes return a proposal that a
  person reviews in a terminal (`sql-review review`, `jira-review review`).
- **`serve --socket <path> --data <dir>`**, the plugin contract.

Standalone, credentials come from the environment, a `.env` or the OS keyring. Under
`serve` they come only from the caller context: `serve` reads nothing from the
environment but the contract's three variables. Standalone use keeps the tool testable
and usable where no node runs, and it keeps the contract a thin layer over a tool that
already works, rather than the only way in.

## The manifest: `tracon-plugin.toml`

The manifest sits at the root of the archive. Unknown keys are refused, since the
contract number versions the shape. sql-review's, as released:

```toml
name = "sql-review"
version = "0.2.0"
contract = 1
exec = ["bin/sql-review", "serve"]
egress = []

[[credential]]
name = "sql-read"
pattern = "sql-read-*"

[[credential]]
name = "sql-write"
pattern = "sql-write-*"
lend = "approved"

[[tool]]
name = "query"
hint = "read"

[[tool]]
name = "describe"
hint = "read"
```

**Write every top-level key before the first table.** A key written after `[ui]` or a
`[[tool]]` belongs to that table in TOML. The parser catches the common case and says
`tracon-plugin.toml: egress is a top-level key; write it before the first [table], not
under [ui]`; inside a `[[tool]]` it is an unknown field.

### Fields

Top level:

| Field | Type | Default | Rule |
|---|---|---|---|
| `name` | string | required | 1-24 lowercase letters, digits or `-`, not starting with `-`. It is the namespace of the tools (`<name>__<tool>`), so it may not be an `[[mcp_server]]`'s name on the node. |
| `version` | string | required | `N.N.N`, optionally `-pre` and `+build` of letters, digits, `.` and `-`; each number at most 9 digits, 64 characters in all. It must match the archive's name. |
| `contract` | integer | required | `1`. |
| `exec` | array of strings | required | The program and its arguments. The first is a plain relative path under `bin/`, and must be a regular, executable file in the archive. No argument may be empty or hold a NUL, or be `--socket`, `--data` or `--node-socket` (or `--socket=…`), which the node appends. |
| `egress` | array of strings | `[]` | `host` or `host:port`: a DNS name or IPv4 address, port 1-65535, or `*.domain` (optionally with a port) for every host under a domain of at least two labels, such as `*.atlassian.net:443`. Shown on cards, never enforced. Nodes before 0.56 refuse a wildcard. |
| `formats` | array of strings | `[]` | Document formats this plugin owns: 1-32 lowercase letters, digits or `_`, each listed once. `markdown` and `source` are the node's own. |
| `node_methods` | array of strings | `[]` | The node methods the plugin calls back (see [Calling the node back](#calling-the-node-back)): `approval/request`, `approval/status`, `notify`, `tools/call`, each listed once. Naming any adds `--node-socket` to the start. |

`[[credential]]`, one per broker credential the plugin may be lent (see
[Credentials](#credentials-lent-per-call)):

| Field | Type | Default | Rule |
|---|---|---|---|
| `name` | string | required | Letters, digits, `-`, `_` or `.`; each declared once. The binding the plugin is lent by default. |
| `pattern` | string | required | Name characters and `*`, which matches any run of characters: the other bindings it may be lent. Repeat the name to match only that binding. |
| `lend` | `"always"` or `"approved"` | `"always"` | `"approved"` lends it only to a call run from a card the operator allowed. |
| `select` | string | none | 1-64 letters, digits, `_` or `-`: the call argument that names the one binding a call is lent, `<name>-<value>` (`select = "profile"` with `profile: "tst"` lends `sql-read-tst`), or `name` itself when the call leaves it out. A binding the argument names that the channel does not bind refuses the call before it is sent. Without it, a call is lent every binding the credential matches. Nodes released before it refuse the key as an unknown field. |

`[[tool]]`, one per tool `serve` lists:

| Field | Type | Default | Rule |
|---|---|---|---|
| `name` | string | required | 1-64 letters, digits, `_` or `-`, without `__`; each declared once. `<plugin>__<tool>` must fit in 64 characters, the longest name a model API accepts. |
| `hint` | `"read"` or `"write"` | required | What the plugin says the tool does. Shown on cards; it never decides anything. |
| `consequential` | boolean | `false` | Never run because something allows it by name: every call is asked, or runs under a grant naming that exact call. See [Consequential tools](#consequential-tools). |

`[ui]`, optional. A plugin with one gets a page in the interface's Plugins section (see
[Plugin pages](#plugin-pages)):

| Field | Type | Default | Rule |
|---|---|---|---|
| `entry` | string | required | A plain path under `ui/` that is a file in the archive. |
| `title` | string | required | Not blank. The plugin's display name: its page is titled with it, and a session's orientation lists the plugin's tools under it (under `name` when there is no `[ui]`). |

Each validation error starts with `tracon-plugin.toml:` and names the field, for
example:

```
tracon-plugin.toml: name: "SQL" must be 1-24 lowercase letters, digits or '-', not starting with '-'
tracon-plugin.toml: contract: contract 2; this node speaks 1
tracon-plugin.toml: exec: "/usr/bin/python3" is absolute; a plugin runs a program it ships, under bin/
tracon-plugin.toml: exec: "--socket" is the node's own to pass; leave it out
tracon-plugin.toml: tool "q__x": name must be 1-64 letters, digits, '_' or '-', without '__'
tracon-plugin.toml: tool "<t>": sql-review__<t> is 72 characters; a tool's offered name holds at most 64
tracon-plugin.toml: credential "sql-read": is declared twice
tracon-plugin.toml: credential `sql` (`sql-*`) shadows `sql-write` (`sql-write-*`, lend = approved): declare `sql-write` first or narrow `sql`
tracon-plugin.toml: egress: "db.internal:70000" is not host or host:port (port 1-65535)
tracon-plugin.toml: formats: "markdown" is rendered by the node itself
tracon-plugin.toml: line 5 (`colour = "red"`): unknown field `colour`, expected one of …
```

The tree is checked too: `exec: bin/x is not executable`, `exec: bin/x is not a file in
the archive`, and `ui.entry: ui/index.html is not a file in the archive`.

The manifest's `[[tool]]` list must name exactly what `serve`'s `tools/list` returns. The
node checks this at every start (see [Readiness](#readiness)). jira-review keeps the two
in step with a test (`tests/manifest.rs`) that compares the manifest with its own
definitions, hints included. Copy that test.

## Releasing

### The release's shape

A release is a GitHub release in a public repository, published as the repository's
*latest* release: the node asks `GET /repos/<owner>/<repo>/releases/latest`,
unauthenticated, so drafts and pre-releases are never installed. It carries:

- one archive per target, named `<name>-<version>-<target>.tar.gz`, for
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and `aarch64-apple-darwin`.
  The node takes the one asset whose name ends `-<target>.tar.gz` for its own host;
  none, or more than one, is refused. Release whichever targets you support; a node on
  a missing one is refused with `<owner>/<repo> <tag> publishes no archive for <target>`.
- `checksums.txt`, one `<sha256>  <file>` line per archive, as `sha256sum` writes it.
  Each archive must be listed exactly once.

An archive holds `tracon-plugin.toml`, `bin/` and optionally `ui/` at its top, and
nothing else: no wrapping directory, no symlinks, no device files, at most 10,000 entries
and 1 GiB unpacked. Only files under `bin/` keep an executable bit; everything else is
unpacked 0644. Build it with `tar -C stage -czf <archive> tracon-plugin.toml bin` (add
`ui` when there is one).

The Linux targets are named `-unknown-linux-gnu`, but a static musl binary is what
sql-review ships under that name, since it runs on any Linux of that architecture.
jira-review builds against glibc. Either works, as long as the binary runs on the
operator's host without libraries they must install.

### What the node checks, in order

1. The owner is in `[plugins] owners` (`cosmicspork` by default, changed only at the
   node), before any request is made: `<owner> is not an allowed plugin owner ([plugins]
   owners)`.
2. The latest release has an archive for the host's target and a `checksums.txt`.
3. The archive's sha256 matches its line in `checksums.txt`.
4. The archive unpacks within the rules above, its top holds only the manifest, `bin/`
   and `ui/`, and the manifest validates against the tree.
5. The archive's name is what the manifest says: `the archive is named <asset>, and its
   manifest says <name>-<version>-<target>.tar.gz`.
6. No other installed plugin owns a format it declares (`format jira_wiki is owned by
   plugin jira-review`), and its name is no `[[mcp_server]]`'s.

The build is then staged at `<state>/plugins/bin/<name>/<version>-<digest16>/`, with the
archive's sha256 and a digest of the unpacked tree pinned in `node.db`, and an install
card is raised naming the build by digest. Nothing runs until the operator allows it.
The tree digest is checked again when the card is allowed, and before every start, so a
build changed on disk is refused (`… changed on disk since it was checked`).

### Attestation

`main` does not verify build attestations for plugins yet. The release's checksum and
the pinned digest are what is checked, and the card says `sha256 <digest> from its
checksums.txt`. The verifier the desktop app uses for its own updates is the shared
`attest/` crate, which is meant to verify plugin releases the same way (plan Decision 6,
U9's second half). Publish attestations anyway, as both real plugins do
(`actions/attest-build-provenance` over the archives), so the check has something to
verify when it lands.

### Update and rollback

- `tracon plugin update [name]` fetches the latest release again. A build with the
  digest already active is "up to date". Otherwise it raises an update card that shows
  the manifest diff (`tool explain added (read)`, `credential sql-read: lent always →
  approved`, `egress db.internal:1522 added`), and the release body as the changelog, up
  to 8,000 characters. The active build keeps serving until the card is allowed. A newer
  build supersedes a card still waiting.
- On approval the new build becomes active, the old one is kept as `previous`, and the
  running processes stop; the next use starts the new build.
- `tracon plugin rollback <name>` swaps active and previous with no card, since both
  were approved. Rolling back is an admin action.
- Settings › Plugins has the same two on each plugin's row. **Check for update** calls
  `POST /api/plugins/{name}/update` with the channel picked in the card's header (the
  node's `[session] default_channel` when none is), then says "up to date" or links to
  the update card it raised. **Roll back to `<previous>`**, shown only when the plugin
  has a previous build, asks for an in-page confirmation naming both versions and calls
  `POST /api/plugins/{name}/rollback`; from off the node's machine a passkey confirms
  it. Both are disabled while a request is in flight and while an update card for that
  plugin waits. A plugin installed from a local archive shows the node's refusal (add
  the new archive again). Installing and removing stay with `tracon plugin`.
- A plugin installed from one source (a repository, or local archives) is not replaced
  from another: `plugin <name> is installed from <source>; tracon plugin rm <name>
  before adding it from <other>`.

Write the release body for the operator: it is what the update card shows beside the
diff. release-please's changelog section does this well.

### Recommended setup: release-please and `release.yml`

Copy sql-review's: [`release-please-config.json`](https://github.com/cosmicspork/sql-review/blob/main/release-please-config.json),
`.release-please-manifest.json` and
[`.github/workflows/release.yml`](https://github.com/cosmicspork/sql-review/blob/main/.github/workflows/release.yml).
jira-review's is the same with a glibc cross-build instead of zig.

release-please bumps `tracon-plugin.toml` with the crate through `extra-files`, so the
manifest's version can never lag the tag:

```json
"extra-files": [
  { "type": "toml", "path": "tracon-plugin.toml", "jsonpath": "$.version" }
]
```

The workflow runs release-please on each push to `main`. When a release is created it
builds the three targets, checks the tag, `Cargo.toml` and `tracon-plugin.toml` agree,
packages each archive, then writes `checksums.txt`, attests the archives and uploads
them to the release:

```yaml
- name: Package
  run: |
    version="${TAG#v}"
    mkdir -p pkg/bin dist
    cp "target/$BUILD/release/sql-review" pkg/bin/
    cp tracon-plugin.toml pkg/
    tar -C pkg -czf "dist/sql-review-$version-$TARGET.tar.gz" tracon-plugin.toml bin
# … in the publish job, after downloading every target's archive:
- run: sha256sum -- *.tar.gz > checksums.txt
  working-directory: dist
- uses: actions/attest-build-provenance@… # pinned by SHA
  with:
    subject-path: dist/*.tar.gz
- run: gh release upload "$TAG" dist/* --clobber
```

release-please creates the release before the archives are attached, so for a few
minutes the latest release has no archive and `plugin add` is refused. The
`workflow_dispatch` input re-runs the build for an existing tag when an upload failed
part way. Pass `secrets.RELEASE_PLEASE_TOKEN || github.token` so the release PR gets CI.

## Process lifecycle

### Start

The node starts a plugin when a channel first needs it: a call, a card's preview, an
`api/` request from its page, or a format conversion. A `tools/list` does not, once the
node knows what the active build lists: it keeps the tools each ready start listed (see
[Readiness](#readiness)) for the active build, in its database and across node restarts,
and offers them from there whether or not a process runs, with the same names,
descriptions and schemas. Only a build the node has not seen list yet, after its
install, an update or a rollback, is started by a `tools/list`, once, on the listing's
channel, to learn them; one that cannot start then offers nothing. Install, update,
rollback and removal each forget what the build before listed.

It runs the active build's `exec` from the install directory with two arguments
appended:

```
bin/sql-review serve --socket <path> --data <dir>
```

- `--socket` is a fresh path in a 0700 directory the node owns
  (`$XDG_RUNTIME_DIR/tracon/p/<12 hex>.sock`, or `/tmp/tracon-<uid>/p/` when that would
  be too long for macOS). Bind it, and remove a stale socket file first.
- `--data` is `<state>/plugins/data/<name>/<channel>/`, 0700, and also the working
  directory. It is kept across updates, and on removal unless `--purge`. It is never
  mounted into a sandbox.

The environment is empty except for three variables:

| Variable | Value |
|---|---|
| `TRACON_PLUGIN_TOKEN` | A fresh random token for this start, held only in the node's memory. Keep only its sha256: that hash is the `instance` every caller context names. |
| `TRACON_NODE_KEY` | The node's ed25519 public key, 64 lowercase hex characters (the node id). Every caller context is signed with it. |
| `TRACON_CONTRACT` | `1`. Refuse to start on anything else. |

No `PATH`, `HOME` or credential is set. stdin is closed. sql-review's `serve` refuses
to start without all three variables and says `serve runs under Tracon (try sql-review
mcp)`, which is the right behaviour for a person who runs it by hand.

### Readiness

The node polls the socket with MCP `initialize` until it answers, within 30 seconds.
It then lists the tools (within 10 seconds) and compares them with the manifest. A start
that lists a tool it does not declare, leaves one out, or lists one twice is stopped and
marked unhealthy at once:

```
its tools disagree with its manifest: lists x not declared; declares y not listed
```

That build is not started again until the operator restarts, updates or rolls it back,
and its tools are not offered, not even as an earlier start of it listed them. The check
runs at every start, including one made after the node offered the build's tools from
what it kept, so a build that lists something else now is caught at its next start.

### One process per plugin and channel

Each channel that uses the plugin gets its own process, with its own data directory, so
nothing one channel's calls leave behind is visible to another's. One process serves
every call on its channel, concurrently, and may be lent several bindings over its life
(`sql-read`, `sql-read-prod`), so keep no credential between calls.

### Idle stop, restarts and shutdown

- A process stops once it has gone unused for the `[[plugin]]` entry's
  `idle_timeout_secs` (900 by default). A call in flight holds it, and the idle time
  counts from the end of the last call.
- It stops when the plugin is updated, rolled back or removed, and at node shutdown.
- To stop it the node sends SIGTERM to its process group, then SIGKILL after 5 seconds.
  Children it started go with it. Shut down cleanly on SIGTERM.
- A start that exits or is not ready is tried again after a wait that doubles from 1
  second to 60. A run that was ready for 5 minutes starts the backoff over. One more
  failure after the 60-second wait leaves it unhealthy until it is next used or
  restarted.

### Logs

Each line of stdout and stderr is kept (at most 4 KiB a line): the last 1,000 in memory,
and all of them in `<state>/plugins/logs/<name>/<channel>.log`, rotated at 1 MiB with
three kept. The node's own lines about the process start `[tracon]` (`[tracon] started
0.2.0 pid 1234`, `[tracon] ready`, `[tracon] stopped: idle`). Log what you would want in
a bug report. Never log a credential or a call's arguments; sql-review logs the caller's
kind, session and channel only.

## The tool protocol

### Transport

MCP over streamable HTTP on the Unix socket, at `/`. The node posts JSON-RPC with
`accept: application/json, text/event-stream` and reads either a JSON body or an event
stream, at most 8 MiB. It sends protocol version `2025-06-18`. If the server sets
`mcp-session-id`, the node sends it back, and a 404 for that session makes the node
start a new one. There is no `Host` worth checking: the socket is reachable only through
the node's private directory, so disable host checks if your MCP library has them
(sql-review: `StreamableHttpServerConfig::default().disable_allowed_hosts()`).

Any MCP server library works. sql-review uses `rmcp` behind `axum`. jira-review answers
JSON-RPC by hand (`initialize`, `ping`, `tools/list`, `tools/call`, and the two
`tracon/` methods) in `handle` in `src/mcp.rs`, which is a fair model when you need the
extra methods: the same function serves its stdio `mcp` mode and `serve`.

A plugin with `[ui]` must also answer HTTP requests under `/api/...` on the same socket
(see [Plugin pages](#plugin-pages)).

### Names

Sessions see each tool as `<plugin>__<tool>`, described as `[plugin <name>]
<description>` (at most 2,000 characters). The input schema is passed through as an
object schema; a `required` that is not a list is dropped. A plugin's tools are offered
on a channel that binds a credential the manifest declares, by name or pattern, or on
every channel when it declares none. The operator can narrow that with `channels` in the
plugin's `[[plugin]]` entry: set, the plugin is offered only on the channels it lists
that the credential rule also allows, and an empty list offers it nowhere. On a channel
it is not offered, nothing of it runs: its tools are not listed, its pages, previews and
format conversions are refused, and no process is started. Execute sessions and external harnesses are offered
them. Plan sessions are offered only the reads the node names for planning
(`jira-review__search`, `jira-review__issue` and `jira-review__transitions`); review
sessions none.

### How a call is decided

The node decides each call before the plugin sees it, exactly as for an
`[[mcp_server]]` tool. First, a name the caller is not offered on its channel is refused
before policy is read, with nothing asked or run: a plugin that is not installed, or a
tool its manifest does not declare, is refused as `no tool named <name> on channel
<channel>; nothing was asked or run`, and a plugin installed but not offered on the
channel says why (`plugin <name> is not offered on channel <channel>: it needs a
credential matching …`, or `…: its [[plugin]] channels in node.toml lists only …`). This
holds for an agent's call and a plugin's `tools/call` alike, and again when an approved
card is run, so a card held while the operator narrowed the plugin's channels runs
nothing. Then:

1. The signed policy bundle, under the `tool` kind and the namespaced name
   (`sql-review__query`). A signed denial wins over everything.
2. The operator's `[[plugin]]` entry, by the plugin's own tool name: `deny`, then `ask`,
   then `allow`, with `*` globs. Its `allow` runs a tool unattended only while the
   signed bundle is trusted.
3. A tool that neither names is asked: the call becomes an approval card holding the
   exact arguments, and runs only once the operator allows it.

```toml
[[plugin]]
name = "sql-review"
allow = ["query", "describe"]
deny = ["drop_*"]
timeout_secs = 120        # one call; 60 when unset
idle_timeout_secs = 900
```

The manifest's `hint` never decides anything: it is the plugin's own word. The shipped
working agreements (bundle v26) allow `sql-review__query`, `sql-review__describe`,
`jira-review__search`, `jira-review__issue` and `jira-review__transitions` by name;
anything else a plugin offers is asked unless the operator allows it. A
[consequential](#consequential-tools) tool is never allowed by name, by either layer.
Policy matches a call's arguments without those its schema marks `x-tracon-format`: prose
says what a call is about, not what it does, so a comment that mentions `git push` is not
refused as one. Enforce your
own invariants in the plugin as well. sql-review and jira-review both refuse a write
whose context has no `approval_id` (their check 7), so a write allowed outright by an
operator's `allow` still does not run.

### Results and errors

The node returns your result's `content` and, when it is an object,
`structuredContent`. A result with `isError: true` reaches the agent as `plugin <name>
reports an error from <tool>: <text>`; use it for a tool that ran and could not do what
was asked. A JSON-RPC error reaches it as `plugin <name>: <message>`; use it for a call
the plugin refuses. A call that does not answer within the timeout is reported as an
unknown outcome, so make writes idempotent or detectable.

Arguments with a float are refused before they are decided (`pass decimals as strings:
a float has no canonical form in the plugin contract`). Declare decimal parameters as
strings.

## The caller context: `_meta.tracon`

Every `tools/call` and `tracon/preview` carries `params._meta.tracon`, signed with the
node's identity key:

```json
{
  "v": 1,
  "plugin": "sql-review",
  "instance": "<hex sha256 of TRACON_PLUGIN_TOKEN>",
  "tool": "query",
  "arguments_sha256": "<hex sha256 of the canonical JSON of params.arguments>",
  "caller": { "kind": "agent", "session": "01a1e94d-…", "harness": "claude" },
  "channel": "personal",
  "approval_id": null,
  "preview_token": "<only on an approved call whose card had a preview token>",
  "issued_ms": 1794900000000,
  "expires_ms": 1794900030000,
  "nonce": "<32 lowercase hex>",
  "credentials": { "sql-read": { "DB_BACKEND": "sqlite", "DB_SERVICE": "apps.sqlite" } },
  "signature": "<hex ed25519>"
}
```

| Field | Meaning |
|---|---|
| `v` | Wire format version, `1`. |
| `plugin` | The manifest name the context is for. |
| `instance` | sha256 of the token this process was started with. A context for an earlier start fails it. The token itself never travels. |
| `tool` | The tool's own name, unprefixed. |
| `arguments_sha256` | sha256 of the canonical JSON of `params.arguments`. Missing or `null` arguments hash as `{}`. |
| `caller.kind` | `agent` for a session the node runs (with `session` and `harness`) or for an external harness (`session: null`, `harness: "external"`); `operator` for the operator API calling on the operator's behalf (both null); `plugin` for another plugin calling through its [node socket](#calling-the-node-back), with `caller.plugin` naming it (a key present only for this kind). |
| `channel` | The channel the call is on. |
| `approval_id` | The card the call was allowed on, or `null` for a call policy allowed outright. |
| `preview_token` | Absent unless the call was dispatched from an approved card whose preview returned a token. See [Previews](#previews-and-formats). |
| `issued_ms`, `expires_ms` | The node issues each context for 30 seconds. |
| `nonce` | 16 random bytes in lowercase hex, fresh for each attempt; a retry gets a new context. |
| `credentials` | What is lent to this call, by binding name: each binding's fields. |
| `signature` | ed25519 over the signing bytes, lowercase hex. |

### Verifying it

**Signing bytes:** the ASCII label `tracon/v1/plugin-call`, then the length of the body
as a big-endian u32, then the body: the canonical JSON of the context *as received*,
with `signature` removed. Compute them on the object you received, not on a struct you
parsed it into, so fields your build does not know (`preview_token` was added this way)
stay covered.

**Canonical JSON:** object keys sorted by byte order, no whitespace, strings escaped as
`serde_json` escapes them, `null` written out (absent and `null` differ), integers only.
A float has no canonical form.

**Checks, in order,** each refusing with no side effect. These are the checks the
contract vectors number:

1. `v` is 1 and `plugin` is your name.
2. `instance` is the sha256 of your `TRACON_PLUGIN_TOKEN`.
3. `tool` and `arguments_sha256` match the call you are serving.
4. The signature verifies under `TRACON_NODE_KEY` (use strict verification).
5. `issued_ms - 5000 <= now <= expires_ms + 5000`, and `expires_ms - issued_ms <=
   60000`.
6. `nonce` is 32 lowercase hex characters and unseen. Remember it until `expires_ms +
   5000`. Only a call that passed 1-5 uses up its nonce.
7. Your own rule: both plugins require a non-null `approval_id` for a write tool.

A minimal Rust verifier for checks 1-6, with `ed25519-dalek` 2, `sha2`, `hex` and
`serde_json`. It accepts both fixture files' valid cases and refuses each of their cases
for checks 1-6; check 7 is the tool's own and belongs where you dispatch writes:

```rust
use std::collections::HashMap;

use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const LABEL: &[u8] = b"tracon/v1/plugin-call";

fn canonical(v: &Value, out: &mut String) -> Result<(), String> {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&map[k], out)?;
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Number(n) if n.is_f64() => return Err("a float has no canonical form".into()),
        other => out.push_str(&other.to_string()),
    }
    Ok(())
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub struct Verifier {
    pub plugin: String,
    pub instance: String, // sha256_hex(TRACON_PLUGIN_TOKEN)
    pub key: VerifyingKey, // from TRACON_NODE_KEY
    pub nonces: HashMap<String, i64>,
}

impl Verifier {
    pub fn verify(
        &mut self,
        ctx: &Value,
        tool: &str,
        args: Option<&Map<String, Value>>,
        now: i64,
    ) -> Result<(), String> {
        let s = |k: &str| ctx.get(k).and_then(Value::as_str);
        let n = |k: &str| ctx.get(k).and_then(Value::as_i64).ok_or(format!("no {k}"));
        // 1-3: for this plugin, this process, and this call.
        if ctx.get("v").and_then(Value::as_u64) != Some(1) || s("plugin") != Some(&self.plugin) {
            return Err("1: version or plugin".into());
        }
        if s("instance") != Some(&self.instance) {
            return Err("2: another start's context".into());
        }
        let mut body = String::new();
        canonical(&Value::Object(args.cloned().unwrap_or_default()), &mut body)?;
        if s("tool") != Some(tool) || s("arguments_sha256") != Some(&sha256_hex(&body)) {
            return Err("3: signed for another call".into());
        }
        // 4: signed by the node, over the object as received.
        let mut unsigned = ctx.clone();
        let sig = unsigned
            .as_object_mut()
            .and_then(|m| m.remove("signature"))
            .and_then(|s| hex::decode(s.as_str()?).ok())
            .and_then(|b| <[u8; 64]>::try_from(b).ok())
            .ok_or("4: no signature")?;
        let mut json = String::new();
        canonical(&unsigned, &mut json)?;
        let mut bytes = LABEL.to_vec();
        bytes.extend_from_slice(&(json.len() as u32).to_be_bytes());
        bytes.extend_from_slice(json.as_bytes());
        self.key
            .verify_strict(&bytes, &Signature::from_bytes(&sig))
            .map_err(|_| "4: bad signature")?;
        // 5: fresh and short-lived.
        let (issued, expires) = (n("issued_ms")?, n("expires_ms")?);
        if now < issued - 5_000 || now > expires + 5_000 || expires - issued > 60_000 {
            return Err("5: expired or too long-lived".into());
        }
        // 6: once only.
        let nonce = s("nonce").filter(|n| {
            n.len() == 32 && n.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
        let nonce = nonce.ok_or("6: bad nonce")?;
        self.nonces.retain(|_, forget_at| *forget_at >= now);
        if self.nonces.contains_key(nonce) {
            return Err("6: replayed".into());
        }
        self.nonces.insert(nonce.to_string(), expires + 5_000);
        Ok(())
    }
}
```

The full versions, with errors a caller can act on and a nonce cache safe across
threads, are sql-review's
[`src/contract.rs`](https://github.com/cosmicspork/sql-review/blob/main/src/contract.rs)
and jira-review's
[`src/contract.rs`](https://github.com/cosmicspork/jira-review/blob/main/src/contract.rs).

### The contract vectors

[`node/tests/fixtures/plugin_contract/`](../../node/tests/fixtures/plugin_contract/README.md)
holds both plugins' vectors, copied verbatim with the commit named. Each file has a
fixed `seed` (a raw ed25519 secret key, `SigningKey::from_bytes(seed)`, not a Tracon
identity seed), its `public_key`, a `plugin_token` and its `instance`, `now_ms`, one
`valid` call and context with its signing bytes and signature, a `valid_preview_token`
case carrying `preview_token` and an `approval_id`, and refused cases (`refused` in
jira-review's file, `tampered` in sql-review's) each naming the check that must refuse it. `node/tests/plugin_contract.rs` checks the node signs exactly
those bytes. Check your verifier against the same file, as sql-review's
`tests/contract_vectors.rs` does with jira-review's, so all three agree on the bytes.
When the wire format changes, both plugins and both fixtures change together.

### Operator and agent callers

The kind tells you who to attribute a change to; the signature is what lets you trust
it. An `agent` call came from a session (`session` set) or from the operator's own
harness through the external door (`harness: "external"`). An `operator` call is a
request from the plugin's own page (tool `tracon/ui`), which the operator opened: it is
decided by policy and the `[[plugin]]` entry as a call to `<plugin>__ui`, never held for
approval, and runs unless one of them denies `ui`. A `plugin` call came from another
plugin's `tools/call`, decided by policy and the callee's entry but never held: one that
would be asked is refused instead. A plugin that ignores the context
still runs; a plugin that records authors, or that gates writes on an approval, must
verify it.

## Credentials lent per call

The node lends a call bindings the channel holds that match a credential the running
build declares, by name or by `pattern`, as `credentials.<binding>`: that binding's
fields as strings. A binding belongs to the first declared credential it matches, and
one no declared credential matches is never lent. No credential is ever in the
environment, so a revoked binding stops at the next call and one process can serve
several profiles.

Because the first match wins, install refuses a manifest in which a credential is
declared after one lent more freely (`lend = "always"` before `lend = "approved"`) that
could match one of its bindings: the earlier one's name or `pattern` matches the later
one's name, or some binding name matches both patterns. Declare the stricter credential
first, or narrow the earlier pattern so the two match no name in common:

```
tracon-plugin.toml: credential `sql` (`sql-*`) shadows `sql-write` (`sql-write-*`, lend = approved): declare `sql-write` first or narrow `sql`
```

**Declare `select` when a call uses one binding.** A credential with
`select = "<argument>"` is lent to a call only as the binding that argument names:

```toml
[[credential]]
name = "sql-write"
pattern = "sql-write-*"
lend = "approved"
select = "profile"
```

An approved `sql-review__write` with `profile: "tst"` is lent `sql-write-tst` (and,
with the same `select` on `sql-read`, `sql-read-tst`), never another profile's account.
A call that leaves the argument out (or passes `null` or `""`) is lent `name` itself
when the channel binds it, and nothing of that credential otherwise. A call whose
argument names a binding the channel does not bind, one another declared credential
owns, or that is not a string of name characters, is refused before it is sent, naming
the binding: `nothing ran: plugin sql-review cannot be lent what this call names on
channel work: the call's profile names binding sql-write-nope, which this channel does
not bind`. A credential lent only on approval is not asked for on a call that is not
approved, so a read on a profile with no write account still runs.

**A request from the plugin's own page (`tracon/ui`) selects by its query.** It carries
no tool arguments, so the argument a `select` names is read from the query parameter of
that name, percent-decoded: with `select = "profile"`, `fetch('api/rows?profile=tst')`
is lent `sql-read-tst`, and a request without `profile` (or with `profile=`) is lent
`sql-read` alone, when the channel binds it. A value naming a binding the channel does
not bind, one another credential owns, or one that is not name characters, refuses the
request with 422 before the plugin is asked, and the refusal is logged even for a read:
`nothing ran: plugin sql-review cannot be lent what this call names on channel work: the
call's profile names binding sql-read-nope, which this channel does not bind`. So does a
query naming the parameter more than once. Other query parameters are yours and not
read. A page's request is never approved, so a credential with `lend = "approved"` is
never lent to one, whatever it selects. The query rather than a header carries the
value because it is part of the `path` the signed context's `arguments_sha256` covers,
so the plugin can verify what the request was lent for, and because a page can put it on
any request, a link, `<img>`, download or `EventSource` included, none of which can set
a header. Send it on every request that reads a selected binding: the page's own state,
such as a chosen profile, is not remembered between requests.

Without `select`, a call is lent every binding the credential matches, and the plugin
picks one itself: on a channel binding eleven `sql-write-*` accounts, one approved write
carries all eleven. The install card warns of each pattern credential declared that way.
Either way, pick the binding from the call: jira-review's `profile` argument selects
`jira-<profile>`, else `jira`, and answers `credential jira-work was not lent to this
call` when it is missing.

A credential declared with `lend = "approved"` is lent only to a call dispatched from a
card the operator approved (`approval_id` set). It is withheld from a call the policy or
the `[[plugin]]` entry allowed outright, and from previews. Use it to keep a capability
out of reach of anything but a reviewed call: sql-review's `sql-write` is the write
account, lent only to an approved apply, while every read runs on `sql-read`.

The install card lists each credential, marks those lent only on approval, says what
one with `select` lends, and warns of a pattern credential without one. An update card
shows a change of `lend` as `credential sql-read: lent always → approved`, and of
`select` as ``credential sql-write: a call is lent every matching binding → the
binding `profile` names``.

An approval card's **Lent on allow** row is computed by the same function the dispatch
uses, from the call's arguments as they would run: when the operator edits the argument
`select` names, the card asks the node again and Allow waits until it answers. A call
the dispatch would refuse shows `nothing: allowing it runs nothing, because …`.

## Previews and formats

### `tracon/preview`

When an approval card for one of your tools opens, the node asks what the call would do:

```json
{ "method": "tracon/preview",
  "params": { "name": "update", "arguments": { … },
              "_meta": { "tracon": { …, "tool": "update", "approval_id": null } } } }
```

The context is built as for the call, with no approval and nothing marked
`lend = "approved"`. Run checks 1-6; check 7 does not apply. A preview sends nothing
upstream and writes nothing. Answer:

```json
{ "previewable": true,
  "summary": "update WRK-12 on https://example.atlassian.net",
  "body": "```diff\n…\n```\n",
  "token": "{\"WRK-12\":\"2026-11-16T09:12:33.000+0000\"}" }
```

- `summary` is the card's heading, one line. A plugin call's title always starts with
  your plugin, the tool and the call's `profile` (`on <profile>`) or `key` argument, as
  the node reads them. Until a preview is shown, the first line of the call's `sql`,
  `summary` or `body` argument follows, bounded; never its other arguments, so bind
  values such as `params` stay off the card and out of the push. Once a preview of the
  call as asked is shown, its `summary`'s first line takes that place in the queue and
  on the page (`sql-review: write on tst · UPDATE t SET a = :v (1 row(s))`). A summary
  that opens with the tool and its target, as jira-review's `update WRK-12 on <site>`
  does, follows the plugin's name alone rather than repeating them. `body` is Markdown,
  at most 256 KiB.
- `token` (a string, at most 4,096 bytes, or `null`) is the version the preview was
  taken against: jira-review's is the proposal's `versions` as canonical JSON. The node
  keeps it with the approval and never reads it; the page sees only its hash.
- `previewable: false` says the plugin cannot show the effect (a read, a DDL statement);
  `body` says why, and the token is ignored.
- `-32601` (method not found) gives the generic card. Any other error is shown on the
  card as your refusal, and the operator can still decide. jira-review uses `-32003` for
  a write that would be refused.
- The node waits at most 15 seconds (or the call's timeout, if shorter).

**Allow requires a shown preview by default.** For a plugin tool, the operator can allow
a card only once the page has shown your preview of the call as it will run. A
`previewable: false` answer counts as shown; a refusal, an error, a timeout or `-32601`
does not. So implement `tracon/preview` for every tool that can be asked, answering
`previewable: false` with a line of why when you cannot show the effect. Reject and
Request changes stay open without a preview. An operator can opt a plugin out with
`require_preview = false` in its `[[plugin]]` entry.

### Applying against the token

The approved `tools/call` carries the token back, signed, as `preview_token`. Apply
against it, not a fresh read. If what it names has moved, send nothing and refuse with a
JSON-RPC error whose `data` is `{"stale": true}` (jira-review uses code `-32002`; the
node keys on `data.stale` alone). The node then fails the approval as stale rather than
uncertain, and the agent calls again for a fresh card.

The node also asks `tracon/preview` once more when the operator allows the card. If the
token it answers differs from the one the operator read, the Allow is refused and the
card reloads with the new preview; nothing is called. Keep your preview a pure read so
this costs nothing, and keep the check above: a change can still land after it.

The operator may edit the arguments on the card. The page then asks you for a preview of
the call as edited, once edits pause, and that preview's token replaces the kept one; the
operator cannot allow the call until the preview shown is of the edited call. So the
approved call's `arguments_sha256` covers the edited arguments and its `preview_token` is
the edit's.

The card also pins the build: when a call is asked, the node records the archive digest
of the active build, and the card describes that build. If the plugin is updated or
rolled back while the card waits, allowing it runs nothing and lends nothing; the
approval fails and the agent calls again on the new build.

### Formats: `x-tracon-format` and `tracon/format/convert`

A string property of a tool's `inputSchema` may carry `"x-tracon-format": "<format>"`,
naming `markdown` or a format a plugin owns. Marked properties are the card's prose; a
marked property inside an array's `items` makes that array the card's list of items.
Mark all of a tool's fields in one format. `key`, `project`, `repo`, `number` and `iid`
are locked on the card. From jira-review:

```rust
json!({ "type": "string", "description": description, "x-tracon-format": "jira_wiki" })
```

A plugin that declares `formats = ["jira_wiki"]` owns that format on the node: no other
plugin with it can be installed. The node renders prose in it by asking the owner:

```json
{ "method": "tracon/format/convert",
  "params": { "format": "jira_wiki", "to": "markdown", "text": "h1. Title" } }
```

Answer `{ "text": "# Title" }`. `to` is `markdown` (from your format) or your format
(from Markdown). It is a pure function: no caller context, no credentials, nothing
upstream, at most 1 MiB each way, within 5 seconds. Anything else is `-32602`. The node
asks the owner's instance on the card's channel, so a conversion is lent nothing and is
not held to the credential rule, but it is refused on a channel the owner's `[[plugin]]
channels` leaves out. If the owner is removed, cannot answer, or is not run on that
channel, the card shows the source.

## Testing a plugin against a node

Unit-test the contract in the plugin's own repository first: the vectors, the manifest
against `tools/list`, and `serve` driven over a real socket (jira-review's
`tests/serve.rs` starts it with a fake Jira and signs contexts with the vectors' seed).
Then try the shipped shape on a scratch node, never on the node you run:

```sh
# A scratch node of its own: state, config and port (docs/guides/install.md, "A scratch node").
export TRACON_STATE_DIR=$(mktemp -d -p ~/.cache) TRACON_CONFIG_DIR=$(mktemp -d -p ~/.cache)
export TRACON_LISTEN=127.0.0.1:7520 TRACON_URL=http://127.0.0.1:7520
cat > "$TRACON_CONFIG_DIR/node.toml" <<TOML
[boundary]
resource_prefix = "scratch"
[gateway]
harness_listen = "$XDG_RUNTIME_DIR/tracon-scratch/harness.sock"
[docs]
preview_listen = "127.0.0.1:7522"
[external]
enabled = true
TOML
tracon setup && tracon serve &

# The archive, named as a release would name it.
cargo build --release
mkdir -p stage/bin && cp target/release/my-plugin stage/bin/ && cp tracon-plugin.toml stage/
tar -C stage -czf my-plugin-0.1.0-x86_64-unknown-linux-gnu.tar.gz tracon-plugin.toml bin
tracon channel create personal
tracon plugin add ./my-plugin-0.1.0-x86_64-unknown-linux-gnu.tar.gz --channel personal
```

A local archive goes through every check a release does except the owner and
`checksums.txt`; it is pinned by its own digest. Allow its card in the scratch node's
interface (`http://127.0.0.1:7520`). Bind the credentials it declares to the channel
with `tracon credential import`, then call its tools from any MCP client pointed at
`http://127.0.0.1:7520/mcp/external/personal` (`tracon external show` prints the line
for Claude Code). Asked calls appear as cards in the same interface.

While you work:

- `tracon plugin ls` shows each process's state, pid, uptime, restarts and last error.
- `tracon plugin logs <name> --channel personal [-n 200]` prints its last lines and the
  node's `[tracon]` lines about it.
- `tracon plugin restart <name> --channel personal` starts it afresh, clearing its
  backoff: what brings an unhealthy one back.
- Rebuilding the archive changes its digest, so adding it again raises a new card. A
  build refused for disagreeing with its manifest stays refused until a new build or a
  restart.

The node's own tests drive a fake plugin,
[`node/tests/support/fake_plugin.rs`](../../node/tests/support/fake_plugin.rs): a small
`axum` server on the socket that answers `initialize`, `tools/list`, `tools/call`,
`tracon/preview` and `tracon/format/convert`, records the environment and every context
it was sent, and switches behaviour (exit, hang, crash, stale, refuse) from files in its
data directory. It is the shortest complete example of the server side, and
`node/tests/plugin_supervisor.rs` and `node/tests/plugin_tools.rs` show what the node
expects of it.

## Consequential tools

A tool marked `consequential = true` changes something the operator wants to approve
call by call, such as moving a Jira issue. The node:

- lists it with a required `operation_id` string added to its input schema (at least 8
  ASCII letters, digits or dashes, `^[A-Za-z0-9-]{8,}$`; a call whose id breaks the
  pattern is refused with an error naming `operation_id` and the pattern), and strips
  it before your plugin sees the arguments, in calls and previews alike, so you never
  need to accept it;
- never runs it because something allows it by name: neither the signed bundle nor the
  `[[plugin]]` entry's `allow` (not even `"*"`) counts, though a `deny` or `ask` still
  applies and a signed denial still wins;
- asks every call, unless a `plugin_call` authority grant names that exact call:
  `plugin:<plugin>__<tool>:<sha256 of the canonical arguments without operation_id>`,
  which the card shows as its grant target;
- records the outcome under the `operation_id`, so a repeat with the same id and
  arguments returns the recorded outcome instead of running again.

jira-review's `transition` is the first consequential plugin tool.

## Calling the node back

A plugin can call the node to raise an approval card and wait on it, push a notification to the operator, or call another plugin's tool. It always does this as itself.

### Declare the methods

Name the methods your plugin calls in `tracon-plugin.toml`. The install card lists them under **Calls the node**, and an update's card shows any that are added or removed.

```toml
node_methods = ["approval/request", "approval/status", "notify", "tools/call"]
```

Name only the methods you use. The node answers no others.

### The socket

A plugin that names any node method is started with one more argument after `--socket <path> --data <dir>`:

```
--node-socket <path>
```

A plugin that names none is not given the argument, so a `serve` that parses its arguments strictly keeps working.

The node serves the socket for that one start:

- It sits in the same private 0700 directory as the plugin's own socket, with mode 0600.
- It is removed when the start ends.
- A restart gets a new path and a new token.

### Authentication

Each request is one JSON-RPC 2.0 call, sent as `POST /` with a JSON body and this header:

```
Authorization: Bearer <TRACON_PLUGIN_TOKEN>
```

The token is the one your process was started with. Any other token, including one from an earlier start, is refused with HTTP 401 and error `-32001`.

The node works out which plugin and channel a request comes from by the socket it arrives on. Nothing in the request body can change that.

### Errors

| Code | Meaning |
|---|---|
| `-32001` | Wrong token (HTTP 401) |
| `-32601` | The node has no such method, or your manifest doesn't name it |
| `-32602` | The method doesn't take those params, including unknown keys |
| `-32003` | Refused. For `tools/call`, `data.decision` is `denied` or `asked` |
| `-32000` | Failed: the called tool errored (`data.tool_error`), or the node couldn't do it |

### `approval/request`

Puts a card in the operator's queue. The card names your plugin as the requester.

```json
{ "title": "Edit risk R1",
  "summary": "Optional **Markdown**.",
  "content": [
    { "format": "word_diff", "title": "Description", "before": "old text", "after": "new text" },
    { "format": "diff", "text": "--- a\n+++ b\n@@ -1 +1 @@\n-x\n+y\n" },
    { "format": "table", "rows": [{ "key": "Owner", "value": "Josh" }] },
    { "format": "markdown", "text": "Why: …" },
    { "format": "source", "language": "json", "text": "{}" }
  ],
  "request_key": "card-42",
  "expires_secs": 3600,
  "on_behalf": { "...": "the _meta.tracon of the call you are serving" } }
```

**Params**

- `title`: one line, 1–200 characters.
- `summary`: optional Markdown, up to 16,000 characters.
- `content`: 1–64 blocks, at most 512 KiB in total. Each block can have an optional `title`. The formats are:

  | `format` | Fields | Shown as |
  |---|---|---|
  | `markdown` | `text` | Rendered Markdown |
  | `diff` | `text` | A unified diff |
  | `word_diff` | `before`, `after` | A word diff the page computes |
  | `table` | `rows` (`{key, value}` strings) | Key–value rows |
  | `source` | `text`, optional `language` | Source text |

- `request_key`: optional. While the card is waiting, sending the same key returns the same card. Without a key, identical content does the same.
- `expires_secs`: optional. It can shorten how long the card waits (60 seconds minimum), but not lengthen it past the node's default.
- `on_behalf`: optional. Pass this when you raise the card while serving an agent's call. The node accepts it only if all of these hold:
  - the node signed it;
  - it was issued to this instance, on this channel;
  - it is less than ten minutes old;
  - its caller is an agent.

  The card then also belongs to that agent's session: the session waits on it, is told the decision, and can read it with `approval_status`.

**Returns** `{ "approval_id", "state": "pending", "new": true|false }`.

The operator can allow the card, reject it, or ask for changes. They can't edit it. Allowing it runs nothing on the node: you act on the decision yourself.

### `approval/status`

```json
{ "approval_id": "…", "wait_secs": 45 }
```

You can also pass `approval_ids` with a list of ids.

It only covers cards your plugin raised on this channel. It waits up to `wait_secs` seconds (at most 45, which is also the default) for a decision, as an agent's status call does.

**Returns** `{ approval_id, state, notes, reason, decided_ms }`. `state` is one of:

- `still_waiting`
- `approved`
- `rejected`
- `changes_requested` (the operator's `notes` say what to change)
- `expired`

With `approval_ids`, it returns `{ approvals: [...], still_waiting }` and comes back as soon as any card is decided.

Keep the ids of the cards you are waiting on in your data directory, so you can ask again after a restart.

### `notify`

```json
{ "title": "Export ready", "message": "…", "path": "/optional/local/path" }
```

Pushes to the operator's devices the same way `notify_operator` does. The node puts your plugin's name in front of the title. It returns the push-service attempts. It can't tell you whether anyone read it.

### `tools/call`

```json
{ "name": "jira-review__search", "arguments": { "jql": "…" } }
```

Calls another plugin's tool as your plugin.

- The callee's caller context has `caller.kind = "plugin"` and `caller.plugin = "<your name>"`.
- The signed policy and the callee's `[[plugin]]` entry decide the call, as they do for the operator API's calls.
- The call is logged on the channel's external log under `plugin:<your name>`.
- It is never held for the operator. A call that would be asked is refused with `data.decision = "asked"`. If you need the operator's say, raise a card yourself.
- Calling your own tools through the node is refused.
- A tool of a plugin that is not installed, or one its manifest does not declare, is
  refused before policy with `-32602` and `no tool named <name> on channel <channel>;
  nothing was asked or run`. One whose plugin is installed but not offered on your
  channel is refused with `-32003`, naming the credential it needs.

**Returns** the tool's MCP result, `{ content, structuredContent? }`.

## Plugin pages

A plugin whose manifest has `[ui]` gets a page in the interface's **Plugins** section.

- **Where it is served.** The node serves the active build's `ui/` files itself, on a
  loopback port kept for the plugin (the first free one in
  `[plugins] ui_port_first..ui_port_last`, 7440-7459 by default), so its origin is its
  own and stays the same across restarts. Every URL is under a capability path
  `/t/<token>/`, minted for one plugin and channel, so every URL your page uses must be
  relative (`fetch('api/x')`, Vite's `base: './'`).
- **What the page may do.** Its CSP allows scripts and connections to its own origin only
  (`default-src`, `script-src`, `connect-src 'self'`; `frame-src blob:`; `object-src
  'none'`; `form-action 'self'`). The interface frames it in `sandbox="allow-scripts
  allow-same-origin allow-forms allow-downloads allow-modals"`: no popups, no top-level
  navigation. It may use its own `localStorage`.
- **Its API.** A request to `api/...` reaches your process for that channel, over the
  plugin socket, as an HTTP request to `/api/...`. The node passes the method, path, body
  and a few headers, never a cookie, `Origin` or `Referer`, and adds a `tracon-context`
  header: the signed caller context as base64url JSON without padding, kind `operator`,
  tool `tracon/ui`, and `arguments_sha256` over `{"body_sha256", "method", "path"}` for
  that request. Verify it as you would a tool call's context, computing the arguments
  from the request. Its `credentials` are lent by the query parameter each `select`
  names (see [Credentials lent per call](#credentials-lent-per-call)). Only
  `content-type`, `content-disposition` and `etag` of your answer reach the page, and an
  answer may be at most 32 MiB.
- **Policy and the log.** A request is decided as `<plugin>__ui`: a signed denial, or
  `deny = ["ui"]` in the plugin's `[[plugin]]` entry, refuses it (the files are still
  served); otherwise it runs, never held. A request that changes something must come
  from the page's own origin, and is logged to the channel's external log under
  `/plugins/<name>/ui`; reads are not logged.
- **Reach.** Pages open on the node's own machine only. Reaching them from the phone
  through the hub comes later.

## Seeing how a tool is answered

`tracon plugin ls` and Settings › Plugins show, for each of a plugin's tools, how a call
to it would be answered: runs, asked or refused, with the rule that decided it, for
example `tool sql-review__query: runs (rule brokered-reads); says it reads`. It is the
same decision that gates calls, made on no particular channel: a bundle rule scoped to
one channel can answer differently there.
