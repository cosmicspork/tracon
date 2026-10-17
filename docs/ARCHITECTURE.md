# Architecture

The rules. This document holds the commitments, invariants, and boundaries that
constrain everything else, with rationale where the reasoning is not obvious. It
describes current constraints rather than implementation history. Planned changes
to these commitments are tracked in [ROADMAP.md](ROADMAP.md).

`tracon`: named for terminal radar approach control. The facility sequences aircraft
and issues clearances; it never flies one. The node supervises and gates; it never
reasons.

## Problem

Agent work driven from TUIs on laptops produces four distinct problems: latency (a
TUI round-trips every keystroke; a chat interface round-trips once per prompt),
environment sprawl (agents running locally against remote environments force
devcontainers on the laptop just to run tests), no enforcement (working agreements
are prose the agent may not still have in context an hour in), and no visibility when
the laptops are closed. The system is a supervisor and control plane between the
human and existing coding agents. It does not implement an agent loop.

## Principles

- **The node supervises, it does not reason.** No model loop in the node. Harnesses
  are drop-in and replaceable; anything that tracks harness features will rot.
- **Enforcement over instruction.** A rule the node cannot enforce is a suggestion.
  Where enforcement is impossible, say so explicitly rather than implying uniformity.
- **Contain execution; ask about what leaves.** Inside the boundary an agent writes and
  runs commands unasked: the boundary holds no credential and reaches only what it was
  granted, so what happens there stays there. The node decides, or asks, where something crosses it —
  egress, a brokered credential, publication. This is the node's own answer to unattended
  work, so it does not depend on any harness's permission mode.
- **Local-first.** A hub outage degrades the system. It does not stop work.
- **Nothing in project repos.** No `.claude/`, no `AGENTS.md`, no tracon files of any
  kind, and no requirement made for tracon's sake: no endpoint or header in the
  application, no seeder, no tool added to its image. A repository supplies what it
  already has — its Dockerfile, its scripts, its tests — and the node supplies the rest.
  Config is materialized into scratch directories at session start.
- **The corpus outlives the tooling.** Memory, docs, work items, and events use
  boring schemas with plain-text export. When the orchestration layer becomes
  obsolete, the accumulated context must survive it.

## Topology

```
                        ┌──────────────────────┐
                        │  hub (cluster)       │
                        │  always-on, relays   │
                        │  ciphertext          │
                        └──────────┬───────────┘
                                   │
                       ┌───────────┴───────────┐
                 ┌─────▼──────────┐   ┌────────▼───────┐
                 │ eligible nodes │   │ PWA / desktop  │
                 │ any host that  │   │ clients        │
                 │ passes checks  │   │                │
                 └─────┬──────────┘   └────────────────┘
                       │ supervised exec
                 ┌─────▼──────────┐
                 │ isolated       │
                 │ harness runner │
                 └────────────────┘
```

Every admitted node dials out to the hub; nothing accepts inbound connections. Host
location does not determine admission: a machine is a node only when it can establish
and verify the harness boundary. The hub is also the relay — both must be always-on,
and the hub already treats work channels as ciphertext-only forwarding, which is what
a relay is. What is preserved from the earlier split design: the work channel stays
opaque to the hub, the policy signing key is never on it, and channel-scoped keys
mean the relay role can be split back out if the hub's host ever becomes the less
trusted option.

## The node

A single statically linked Rust binary that supervises harnesses, speaks the mesh
protocol, serves the embedded SPA, evaluates policy locally, brokers credentials and
tools, and persists everything to local SQLite. Explicitly not its responsibilities:
model inference, an agent loop, prompt construction beyond assembling injected
context, and any business domain.

Build rules: static musl on Linux `x86_64`, and Apple Silicon macOS, SPA embedded
so every node serves the identical bundle, log to stdout, no self-daemonizing, clean
SIGTERM, idempotent restart. Supervision is external — the platform's service
manager, which `tracon service install` or the desktop app sets up. Static
musl is also why consulta stays a Python sidecar: every Rust Oracle crate wraps a
dynamically linked glibc blob, and `oracledb` thin mode is a pure-Python wire
implementation. Recorded so the rewrite is not re-decided.

### Configuration

`node.toml` is the operator's file, and the node writes it through one path,
`config::edit`. Every writer (Settings, the provider and repository editors, mesh pairing
and enrolment, an approved egress ask or repository setup, the CLI) states its change as
typed operations: set a key, remove one, insert or remove an entry of an array of tables.
The edit applies them to the file as it stands with `toml_edit`, so comments, key order
and formatting survive and nothing the operator did not change is rewritten. Writing the
whole struct back instead turned every default into a pinned value and dropped every
comment.

Under one lock, an edit reads the file and checks the version the caller decided against
(the SHA-256 of its bytes; a mismatch is a conflict carrying the current hash), applies
the operations, and loads the result with every check a start makes. A result that would
not load is refused with the file untouched, so a write can never leave a node that will
not start. It then records a pending `config_audit` row (who, when, the hashes before
and after, each changed key with its values), renames the synced new file into place and
marks the row landed; a failed write withdraws its row, and a start settles a row an
interrupted edit left pending by the file's hash. The lock is a process mutex plus a
lock file beside `node.toml`, so the CLI and the serving node serialise too. The
`[[repo]]` table changes under one more lock, held from reading the live table to
setting it again, and its entries are matched by `path`, so a removed or moved entry
takes its own comments with it. The operator's devices get a push through the ordinary
operator notification path, keyed by the audit row. A writer whose operations depend on
what the file holds now decides again when another edit lands first.

`config_schema` describes every key: whether a change applies live or at restart,
whether writing it is an admin action (every key is), whether it names an executable the
node runs or a host path it reads or writes (and which rules of the path guard hold it),
whether its value is a secret the audit log redacts, and whether it is changed at the
node only (`host_only`). A test
walks the `Config` structs' own `Deserialize` and fails on a key without an entry, so a
new setting cannot arrive without saying which it is. The one other writer is the
load-time migration of a retired harness, which rewrites the file once and keeps the
original beside it.

**Live keys and the restart owed.** Every part of the node holds the `Arc<Config>` it was
given at start; inside it a `ConfigHandle` carries the newest file the node has taken in.
An edit through the node swaps it at once, and a file changed by another process (the
CLI, an editor) is taken in by the 15-second tick once two looks in a row see the same
version. That look takes the `[[repo]]` writers' lock, so it can wait behind an
`update_repos` that is itself waiting on `node.toml.lock` held by a CLI process; the tick
is delayed, not lost, and carries on once the CLI releases the lock. Code reads a live key through
`Config::live()`, which is the configuration the node started with with only the
schema's live keys laid over it from the newest file, so a reader that also touches a
restart key still sees what is running. Live today: `[[repo]]`, the `[session]` defaults
(budget, permission and approval timeouts, harness API timeout, default channel, claim
grace, suspend-after), `[publish]`, `[github] dispatch`, `[supervision]`, `[review]`
limits, `[notify] contact`, `[memory] promote_at`, provider prices,
`[runtime] approved_images` and the `[docs]` export keys. Everything else (listeners,
the boundary and runtime, the mesh, providers' credentials and upstreams, the harness,
`[auth]`, `[ui]`) holds until a restart. The node remembers the file it started from and
reports the restart-only keys that differ from it now (`GET /api/restart`, and a
`restart` stream frame), which the interface shows as one banner on every screen.

**Restart over HTTP.** `POST /api/admin/restart {mode, expected_hash}` restarts the node
into the file the caller read; a file that changed since, or that would not load, is
refused, and the file is read again just before the restart, so one edited while the
restart waited is not restarted into. `when_idle`, the default, refuses new sessions and
turns (OpenCode commands and compactions included) and new `submit_review` calls, waits
(30 minutes unless the request says otherwise) for running turns, running required checks
and every review submission whose checks have not all run (they run one after another,
so a review between two checks is still in flight), then restarts; if they have not finished it gives up, takes work again and
pushes why. `now` answers with the live sessions it would end until the body confirms
them. What restarts the node is its supervisor: the fixed user service where it runs
under one (on macOS the node hands the unload and reload to a `tracon service restart`
in a process group of its own, since launchd stops the job's whole group when it unloads
it); where the deployment declares that the container is restarted on exit
(`TRACON_RESTART_BY_EXIT=1`, which the Kubernetes manifests set) the node drains, shuts
down as SIGTERM would and exits 0 for the kubelet to start it again on the same volume
and pod IP. Anywhere else the request is refused, because nothing would bring the node
back. A restart handed to the supervisor that has not happened two minutes later (the
node still running and not stopping) is given up the same way, so a failed restart never
leaves the node refusing work for good. The route is an `AdminAction`: free at the node,
and from anywhere else a passkey assertion bound to exactly this request, so the phone
can ask while a bearer token never suffices off the node.

**Last-known-good.** `serve` first takes an exclusive lock on its state directory and one
beside its `node.toml`; a second `serve` on either (a scratch node pointed at the real
config directory included) stops there, before it can count as anything. Each start then counts itself,
before the configuration loads, in `boot-attempts.json`, keyed by the file's hash; serving
for 30 seconds, or a clean stop, clears the count, so only a start that dies before it is
serving is a failure. A node that has run five minutes keeps the file it started from as
`node.toml.last-good`, beside it with its permissions, and every five minutes after keeps
the file as it is then when it loads, has held still since the last look, and differs from
what is running only in live keys, so last-good follows live edits rather than going
stale; a version is kept only once the running node has taken in its live keys. A start
that finds three failed starts in a row of one file restores last-good only when last-good
itself loads and the file is to blame: it does not load, or a restart-only key differs from
last-good's (both read as a start reads them, the retired harness migrated). The file is
read again under the edit lock first, and one edited meanwhile is left alone. It then keeps the failed file as `node.toml.failed-<ms>` (through a symlink,
under the edit lock), puts last-good in its place, and on the next successful start
records a `config_audit` row (actor `last-known-good restore`) and pushes it. A file equal
to last-good is never replaced, so the restore cannot loop. Undoing one is in
[RECOVERY.md](RECOVERY.md).

#### Editing from elsewhere

Every key is editable from wherever the operator is (`http::config_api`), except a few
the schema marks `host_only`. The state mount is changed in the file on the host and
never by an edit, since it decides where agents can write. The gateway's harness socket
path (`gateway.harness_listen`, whose directory the node creates and mounts writable
into the gateway container) and the images the node runs with its own sockets and
credentials (`boundary.gateway_image`, `boundary.harness_image`,
`runtime.kubernetes.harness_image`) are outside the path guard's rules, so they are
changed at the node, by hand or by an edit made there, and refused from elsewhere.

`GET /api/config` returns the file as written (`document`), as it loads with every
default filled in (`effective`), the redacted document as TOML text, the file's `hash`,
and the schema of every key. A secret's value is never sent, and neither is the value
of a key the node does not read (the schema holds an unknown key to the secret rule,
since a retired section can hold a credential); the text is rendered from the redacted
document rather than sent as written, because a comment can hold anything, a
commented-out key among it. An error that quotes the file is said without the quoted
lines. Reading needs only the cookie.

`PATCH /api/config {expected_hash, ops}` takes typed operations (set a key or an array
entry to a JSON value, remove a key or entry, insert an entry into an array of tables,
reorder one), at most 256, and maps them onto `config::edit`. `expected_hash` (or
`If-Match`, which the admin `op_hash` already binds) is required. Before anything is
confirmed the node answers only what it can from the request and its own state, in an
order that tells a caller nothing about a hidden value:

1. What the operations name, from the request alone: a string or a key holding a
   control, format or separator character (`\n`, `\t`, `\r`, a zero-width space, a
   direction override: Unicode Cc, Cf, Zl, Zp), which a prompt could not show as
   written; a key the node does not read; a secret's placeholder sent back as its
   value; a host-only key (`422`, key by key). The Settings save, the repository
   table and a new provider refuse such characters the same way.
2. A stale version: `409` with the hash now.
3. Operations that do not apply, a file that would not load (a builder table included),
   and keys a whole-table set or a new array entry would add that the node does not
   read (`422`).
4. A change that changes nothing writes nothing and confirms nothing, unless the request
   writes a secret's value: then it is confirmed like any change, so "nothing changed"
   never confirms a guess.

Then the request is an admin action: off the machine it is confirmed with a passkey
bound to its exact bytes, and the node words the prompt (`http::config_prompt`). It is
the one thing a phished confirmation would be judged by, so what could change what the
node runs, trusts, reads, sends or reaches comes first, in full: programs and host
paths, secrets (named, value not shown), host-only keys, keys the node does not read,
`auth`, `mesh`, `runtime`, `boundary`, `consulta`, `[harness]`, the gateway's allowlist
and socket, a provider's upstream, credential and sign-in, a repository's path,
preparation, lent credentials, egress, image, Dockerfile, context and checks, a
service's name, image and command, the checks run on submissions, plugins, the embedding
switch, endpoint, key and provider, the GitHub dispatch list, the OpenCode UI and
preview origins and listeners, the gateway's forward port, external harnesses, the
default and export channels, the push contact, how commits reach the forge, and
whether a terminal's output is captured. A value is never cut: a list shows the
elements it gains and loses, counted as a multiset so a second copy shows (`gateway.allow_hosts: + "evil.example"`),
a key the file did not write is compared with its default ("(from the default)"), a
value shows its old and new (`repo[0] ("/src/app").path: "/src/app" -> "/src/other"`),
an `http://` URL says "(cleartext)", and every string is quoted with `"`, `\` and any
character a prompt could not show written as an escape (`\n`, `\t`, `\u{200b}`), so
nothing in a key or value can pass for the prompt's punctuation or hide in it. A key
written under a built-in provider the file did not name until now says that it
creates the table and replaces the built-in provider. An edit
whose sensitive part does not fit 600 characters is refused, to be split. Routine keys
(`[memory]`, `[review]` limits, the session budget and timeouts, supervision's timeout
and dependency inputs, a provider's models and prices) follow, as many as fit, and how
many more; room for that count is kept, so it is never what is cut. An entry of an array of tables is named by its own `path`, `name` or `id` as
it was before the edit, with the fields that change ("repo "/src/app" added:
dockerfile = "Dockerfile", prepare = ["composer install"], prepare_credentials =
["NOVA"]"), and a key inside one by that entry; an entry under a key the node does not
read is named only by its index. A map entry's name is capped at 24 characters and
every part is cleaned of control, format and bidi characters. If a line ever has to be
cut on its way to a prompt (a device approval's included), it ends in an ellipsis.
Because the request names the version it was decided against, what the
prompt shows is exactly what the write changes. The path guard runs when the edit is
written, after the confirmation, so what it knows about the host's files (who owns a
path, whether it exists) is not answered to a cookie alone; its refusals come back as
`422` with each refused key and its reason. An edit that touches `[[repo]]` moves the
live table with the file, as the repository editor does.

**A peer's `node.toml`.** `GET` and `PATCH /api/nodes/{id}/config` are the same two
routes for another node, over the mesh (`ConfigRead`, `ConfigPatch`). The peer answers
only if its own `[mesh] admin_peers` names the asking node: a peer's settings are
visible and editable only from nodes it names there. Otherwise the refusal says so
(`not_admin_peer`) and the interface shows why, and nothing of the file. A read returns exactly what the peer's own `GET /api/config`
would. An edit is decided first on the asking node against the file as the peer shows
it: the version it names, the characters it holds (none a prompt could not show), keys
a peer may change, and the prompt, worked out from that file by the same code and
prefixed `On <peer name> (<first 8 of its id>…): ` within the same 600 characters (the
name is the peer's own word; the id is not). A peer may change everything a remote edit
with the node's own passkey may, except who may reach or administer the node and where
it sends its credentials (`config_schema::NOT_BY_PEER`): all of `[auth]` and `[mesh]`
(`admin_peers` and the hub among them), a provider's `upstream`, `credential` and
`login`, and the embedding `base_url` and `api_key_file`. Those, and the host-only
keys, are changed at the node, or remotely with its own passkey; a peer that could
change them could strand the node's own operator, keep its grant past its removal, or
send a credential to a host of its choosing. Then it is confirmed there (a passkey off that node's machine),
and the asking node signs an admin statement for exactly that edit on exactly that
peer (see [Mesh frames](#mesh-frames)). The peer checks the statement, then runs the
edit as its own `PATCH` would, as an editor held to the same exceptions: the
version, the operations, the keys, the path guard against its own host, the audit row
(actor `peer <id> for <operator session>`, or `for local`), the push, and the live keys
applied at once. An edit is never queued: a peer that is away is refused at once, and
one that arrives after its two minutes is refused there. A peer's refusal is relayed
as the peer's, never as the asking node's own answer: `peer_refused`, the peer's message
cleaned and capped beneath the asking node's line, its kind and fields as data, a
conflict's hash only if it is one, and a status other than `404`, `409`, `412`, `422`
or `428` turned into `403` or `502`, so a peer cannot have the interface ask for a
passkey or a sign-in by answering `step_up_required` or `401`.

The routes that wrote `node.toml` (or what a harness runs) at the machine only are admin
actions now, confirmed the same way, each with a prompt the node words from its body.
The Settings save (`PUT /api/config`) and the repository table each require the `hash`
they were read with and write only over that version, so what is written is what was
confirmed; their prompts are made as above. Building a repository's image names the
repository; adding a provider is decided against the file and prompted as any edit
(the full upstream, the credential, the allowlist entry it adds), and its name and
credential must be short and plain; the launch manifest's instructions and agents
check their kind, name and channel before anything is confirmed, and the prompt shows
the text's size, the start of its SHA-256 and its first 200 characters, newlines as
escapes (any other character a prompt could not show is refused); starting, joining
or leaving a mesh names the hub, quoted. What stays at the machine (`auth::Loopback`) is
what acts on the host itself rather than on configuration: importing a skill package
(it reads a host directory the caller names), the policy signing key and bundle
installation, the service lifecycle (install, uninstall, restart), the storage sweep and
the boundary re-check.

Two keys deserve a word, since a confirmed edit can set them. `[auth] trusted_proxies`
decides only whose `X-Forwarded-For` the node believes when it tells callers apart: set
to `0.0.0.0/0`, every peer is believed, so a caller chooses the address its per-address
login, ceremony and device-approval limits count against and the address an approval
page shows (the global limits still hold). It never makes a request local, since any
forwarding header makes it remote. `mesh.hub_url` points the node at a hub after a
restart. A hub sees routing metadata and ciphertext, can drop or delay what it relays
and serve stale policy, but cannot read a work channel's content, forge a peer's signed
command or serve a policy bundle it did not get signed. A hub of an attacker's choosing
therefore costs this node its mesh and shows the attacker which peers it talks to and
when. Both changes are in the audit log and the push.

#### The path guard

Remote configuration makes every key writable by whoever holds an admin session, and
agents write files: their workspaces, their scratch, the state volume a pod shares with
the node. A program key pointed at one of those files would run an agent's bytes as the
node; `embed.api_key_file` is read and sent to `embed.base_url` as a bearer token, so
pointed at the identity seed or an SSH key it would hand either to a host the writer
chose; a directory the node writes into or replaces could be one something else reads or
runs. `config::path_guard` holds each such key to rules for what it names
(`config_schema::PathKind`), and where a rule can be an allowlist, it is one:

- **Programs** (`boundary.podman`, `publish.gh`, `publish.glab`, `publish.git`,
  `consulta.command`, `runtime.kubernetes.builder.buildctl`): an absolute path without
  `..`, or a bare name looked up on the node's `PATH`, whose file name is exactly one the
  key runs (`podman` or `podman-remote`, `gh`, `glab`, `git`, `uv` or `consulta`,
  `buildctl`), so a shell or interpreter installed somewhere trusted cannot stand in for
  it. Every symbolic link on the way is followed, by hand, and the directory each link
  lives in counts as much as the target. The target is a regular file with an execute
  bit, and neither it, the path as written, nor any link's directory is inside a place an
  agent can write. The file and every directory above it (and above each link) belong to
  root or the node's user, and none is writable by every user or by a group other than
  root's, macOS's `admin`, or the node's own, unless it is a sticky directory like
  `/tmp`. Outside a package manager's prefix (`/opt/homebrew`, `/usr/local`,
  `/home/linuxbrew/.linuxbrew`, `~/.local/bin`, `~/.cargo/bin`) none may be owned by the
  node's user, or writable by its group, either: an owner can always make a file
  writable, and this closes the chain where the node is made to write a file and then run
  it. A program whose path as written is inside a prefix is held to the prefix's rules
  even where a link there points outside it, as uv's tool links into `~/.local/share/uv`
  do. (The node's group counts as the node: on Kubernetes it is the volume's `fsGroup`,
  which makes the whole volume group-writable.) A bare name skips a `PATH` entry that
  fails these rules and runs the next one that passes, and every guarded program is given
  the node's `PATH` less every such entry, and less empty and relative ones, so a program
  that runs another (git its helpers, uv its tools) does not find a planted one.
- **`consulta.args`** decides what `uv` runs and what consulta is told, so with `uv` it
  must read `run [--frozen|--locked|--no-sync|--offline|--no-dev|--quiet|--no-progress]
  [--project <dir>] consulta`, with nothing after `consulta`: the node appends the
  query's own arguments, and consulta's options include where it writes its output.
  `--with`, `--python`, `--directory`, an index, a script or any other program are
  refused. The project must be an absolute directory, not `/` or a system directory, out
  of the places an agent can write, owned as a secret file is (the node's own user may
  write it: the node image's `/opt/consulta` is the node's), and must hold a
  `pyproject.toml` naming the project `consulta`. With `consulta` itself as the command,
  `args` must be empty. The node runs consulta from its state directory, since consulta
  reads the first `.env` it finds walking up from its working directory and would
  otherwise take settings from wherever the node was started.
- **Secret files and directories** (`embed.api_key_file`,
  `runtime.kubernetes.builder.do_token_file`, `.tls_dir`, `.docker_config`): only inside
  a directory set aside for the node's secrets, `<config dir>/secrets` (on macOS inside
  the directory that is both configuration and state, which counts as the node's own
  there) or `/etc/tracon` (where a pod mounts its Secrets); absolute; never inside a place an agent can write;
  never one of the node's own secrets under the state directory (`node.db` and its
  journal, `node-identity.seed`, `credentials.sealed`, `credentials.toml`, `policy-key`;
  matched by path, through links, and by file identity, so a hard link is caught); never
  under `/proc`, `/sys` or `/dev`; never in the operator's usual credential stores
  (`~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.docker`, `~/.netrc`, `~/.claude`,
  `~/.config/gh`, ..., and a pod's service-account token); and owned and writable as a
  program's directories are, except that the node's user may write them. A secret
  directory may not hold the state directory.
- **The export directory** (`docs.export_dir`): only the node's own export area
  (`<data dir>/tracon/exports`), or an existing directory that holds a `.tracon-export`
  file and is outside every place an agent can write. The marker says the operator set
  the directory aside; it is not proof of that on its own, since an agent can write one
  into its own volume, which is why the place rule applies too. Never `/`, a system
  directory (`/etc`, `/usr`, `/var/lib`, `/System`, ...), the home directory itself,
  inside the configuration or state directory or a worktree root, a directory holding
  the configuration or state directory, or inside or holding a package manager's prefix.
  The export itself is written through the directory it checked: the directory is
  opened as itself (`O_NOFOLLOW`) and its descriptor compared by identity with what the
  check's own walk found, and from then on everything goes through that descriptor
  (`archive` opened from it the same way, documents read with `O_NOFOLLOW`, written to a
  new file with `O_EXCL | O_NOFOLLOW` and renamed with `renameat`, stale ones removed
  with `unlinkat` after an `lstat`). A link where a document goes is refused, and no
  link repointed after the check can move the export somewhere else.
- **The UI bundle directory** (`ui.opencode_bundle_dir`), which `setup` replaces whole:
  as the export directory for what it may not be, outside every place an agent can
  write unless it is in the node's own state or data directory, and only inside those,
  a directory that does not exist yet, or one already holding a bundle (one the node
  would serve, or one tracon installed, which it marks with a file beside it).
  Separately, `setup` never moves aside or deletes a directory that holds anything but a
  bundle, whichever key or variable named it, and makes its staging directory fresh and
  writes the marker without following a link.
- **Roots** (`session.worktree_root`, `external.repo_roots`): never `/`, a system
  directory, inside the configuration directory, a directory holding the configuration or
  state directory, or inside or holding a package manager's prefix.
- **The state mount** (`runtime.kubernetes.state_mount`): never `/` or a system
  directory, and never changed by an edit at all: it decides where agents can write, so
  it is changed in `node.toml` on the host. It holds the state and configuration
  directories by design, and everything under it counts as a place an agent can write,
  except the configuration directory's `secrets` (a harness pod mounts only the claim's
  top-level `tracon-*` volumes, never `config/`).

The node's own areas (the export area, the state and data directories for the bundle,
the configuration directory's `secrets`) are an exception to "never in a place an agent
can write" only where they themselves sit in one (macOS keeps them all in the state
directory; a pod keeps them on the claim), and the exception holds only for a path that
resolves into the area and passes through no link kept in any place an agent can write.
System and operator links on the way (`/var` to `/private/var` on macOS, `/home` to
`/var/home`) are fine; a link an agent made is not, so a path spelled in an agent's own
volume and linked into an own area is refused like any other path in the volume.

A place an agent can write is the state directory, the state volume, the session
worktree root, Podman's container storage (the data directory's `containers`, and
`/var/lib/containers` on Linux, where named volumes and container layers live), and the
node's runtime directory. In a pod the state volume is found from the running process
(the filesystem the state directory is on, or its nearest existing ancestor on a first
start, where `KUBERNETES_SERVICE_HOST` is set and the file says `runtime.kind =
"kubernetes"`; never `/` or a home directory), not only from `state_mount`, and an edit is judged against the places the
running node started with as well as those either file names, so two edits cannot move
the volume and then use what was on it. Every place is matched as written, as it resolves, and by the
identity of the directory on disk, so `/tmp` and `/private/tmp`, `/home` and
`/var/home`, or another letter case on a case-insensitive volume, are one place. The
home directory is `HOME` and, when it differs, the one the password database gives.

The rules apply at three points. An edit (`config::edit`) is refused when it makes any
guarded key break a rule it did not already break: what the old file broke is judged by
its own places, what the new one breaks by the places either file names, so moving the
worktree root away from a program in the same edit does not unmake it, and moving it
onto a program's directory is refused. A value that names nothing yet, somewhere only
root or the operator could put it, is allowed. A start (`serve`) does not refuse a file
that breaks a rule, because a node that cannot start cannot be reconfigured remotely
either; it warns about each key, and about each `PATH` entry the guard refuses. And at
use: every spawn of a guarded program (each of Podman's multi-step volume transfers, as
one) checks it again just before it runs and runs the path the check returned, so a
binary swapped or a directory loosened after the edit is refused there; the embedding
key file, the builder's token file and the registry's `config.json` are opened, then
checked, then compared by identity with what the check's own walk found, and read from
what was opened; the builder's certificate directory is checked with every `buildctl`
spawn, which reads it itself; the export directory is checked and opened as described
above before every export, and the bundle directory is checked before `setup` replaces
it. At use a secret file outside the
dedicated directories is still read, so a key file placed by hand before the guard
existed keeps working (with the start's warning) until it is moved; every other rule
holds there.

What remains between a check and the `exec` or `open` after it is a window of
microseconds that only someone able to write the program's directories can use: root,
or, inside a package manager's prefix, the operator's own user. The guard does not
cover programs the node spawns by a fixed name other than `git` (`systemctl`, `id`),
which rely on the node's own `PATH`, which no key sets (the review capture's, the
session checkpoint's and the skill reader's `git` go through the guard);
configuration files a trusted program reads from the home the node gives it, which on
Kubernetes is on the state volume (the publish path's `git` is told to ignore them);
Podman storage moved elsewhere by `storage.conf`; the gateway's systemd unit, which
names the podman checked when setup or recovery wrote it, with the guard's `PATH`, and
which systemd runs later without asking the node; and image keys (`boundary.harness_image`, `[[repo]] image`,
`[[service]] image`), which choose what runs inside the boundary, not on the host, and
are held by digest pinning instead. The guard is a backstop behind the passkey every
admin save needs, the audit row and the push.

## Harness control

**The adapter trait is the interface**, and each harness reaches it over its own
protocol: OpenCode over its native server API, Claude Code over the stream-json
control protocol. They are functionally equivalent at the permission gate, which is
the only place that has to be true. The trait is the part of the system that rots:
version-pin harnesses per node, record the version on every session, check
compatibility at session start, and let an unknown harness id refuse to start rather
than fall back. A harness declares its own state layout and config files; nothing
outside the trait may spell a harness's name.

Which harness daily work runs on is a roadmap decision, not an architectural one:
Claude Code today; OpenCode is driven over its v1 session routes because the v2 runner
offers the model none of the node's MCP tools (`reference/opencode-v1.18.30/README.md`,
findings 22 and 23), and has a real session still to run that way. The gate below is
the same for both. That last rule is what made the 2026-09-13 cutover cheap. tracon's first harness was
omp, driven over the Agent Client Protocol; removing it took the adapter, its image,
its provider wiring and its catalogue workarounds, and nothing else. What survived is
the shared vocabulary the adapters translate into (`node/src/adapter/types.rs` — tool
calls, permission options, usage), because both remaining harnesses have the same
things to say. Sessions the retired harness ran are archived read-only rather than
deleted; see `tracon session archive-legacy` in RECOVERY.md. Two concrete adapters
behind one trait is the whole ambition; a plugin system for harnesses is not.

### The OpenCode boundary

OpenCode is a *server*, not a pipe, and that changes where the seam is
rather than what it guarantees. A supervised session is one `opencode serve` in its
own runner, and everything between it and anyone else is node-owned. In order:

- **The adapter owns the process and its environment.** One server per session, bound
  to loopback with mDNS off, a per-session `HOME`, all four XDG directories and its own
  database, a read-only config the node wrote outside the worktree, and a server
  password the node asserts is set. Nothing is discovered: the image carries no ambient
  config, the model catalogue is declared rather than fetched, and the gateway's URL is
  written into the provider entry, the catalogue provider and every catalogue model —
  the session path resolves a model's host from the catalogue, so naming the gateway in
  one place only leaves a silent way around it.
- **Every route is classified before it moves.** The API a browser or another node
  reaches is a mount on the operator router, so the operator's own guard answers first;
  behind it a matrix derived from the release's own route table decides read, mediate,
  refuse, or synthesise, and denies by default anything unlisted. The workspace
  directory is pinned on every request — query, header and body — and the harness
  credential is injected on the node, so it never reaches a client.
- **Permissions are all-`ask`, and the node answers them.** No server-side hook gates a
  tool before it runs, so enforcement is containment plus a ruleset the node writes that
  asks for every tool class. Policy is evaluated on the node and the reply is always
  `once`; an `always` is rewritten and the attempted broadening recorded, and the route
  that would edit the ruleset is refused.
  Adapters normalize each request into a canonical action, semantic kind and resource
  or command before that policy decision. Display titles never participate in policy;
  an unknown action or a known action without its required target has no exempt kind
  and therefore remains an operator decision unless an explicit deny matches.
- **Session events are synthesised, not proxied.** The harness's server-wide stream
  has no durable replay and no scoping, so the node never opens one on a browser's
  behalf: it serves that route itself from its own bus, filtered to the one session the
  caller's cookie names and numbered so a client resumes rather than restarts.
  Correctness does not rest on the stream at all: the adapter drives the v1 session
  routes (the ones that offer the model the node's MCP tools, finding 23), claims each
  part's milestone in the store exactly once, and after any drop re-reads the harness's
  own message list, so a repeat is dropped and a gap is filled from the record the
  harness keeps. The synthesised stream is a view of the same reading.
- **State is the node's to fence.** Two processes can open the same harness database,
  and an older build opens a newer one silently, so each session's state directory has a
  single writer, its identity is recorded at launch, and backup, upgrade and restore are
  the three verbs under "Data lifecycle".
- **What a session was launched with is a manifest**, not a directory the harness
  discovered; see below.
- **The native UI is a client of that boundary, not a hole in it.** It is served from a
  second origin under tracon's own CSP, entered through a single-use capability the page
  exchanges for a cookie, and it can reach only the mediated mount for the one session
  that capability named. The window and frame rules are under "Clients".
- **A terminal is an authority grant**, bound to one session and its workspace,
  default-`ask` and revocable, with the spawn rewritten rather than forwarded and a
  node-minted, owner-bound, single-use ticket for the socket. What is recorded is that a
  terminal was opened, with what shell and where: a terminal is not a per-command ledger,
  and saying so is better than implying one.
- **Another node's operator reaches all of it over an owner stream**, decided again by
  the owner's own gateway, policy and intent row; see "Owner streams".

Rules learned against real harnesses, kept as rules:

- **Deny-by-default is correct.** A permission request the client cannot answer is
  denied, not silently approved. This is how local-first and fail-closed coexist:
  policy runs on the node, so auto-allowed work continues through a hub outage while
  anything needing a human blocks. Degraded means slower, never silently permissive.
- The node declares client-side filesystem reads unavailable, so the harness reads
  inside its runner rather than turning the node into a file server.
- Budget accounting charges the prompt tokens a call actually processed — fresh input
  and cache writes — and the output, not the cached context a long session rereads on
  every step; cache reads are recorded beside the charge.
- A harness that runs a server is inventoried before it is trusted: the recorded
  cautions in `docs/reference/opencode-v1.18.30/` are the upgrade checklist, not a
  one-time read, because a release that moves which stack resolves a request moves
  this boundary silently.
- **A harness's language tooling is part of its image, named by absolute path.**
  Left to itself a harness downloads its language servers and formatters on first
  use, from a network that is denied — and the three formatters that install
  themselves have no flag that stops them, so naming the command is the only lever.
  The image bakes a small, explicit profile (the languages this project's own work
  uses), records it with digests at a fixed path, and the node renders the harness's
  server and formatter configuration from the same committed profile, disabling by
  name every builtin the image does not carry. Overriding a builtin's command
  discards the builtin's initialization, so each override carries its own copy.
  Anything a harness resolves for itself at run time — a plugin package, a search
  binary — is pre-populated in the image and copied into the session's own tree
  before the harness starts, so resolution succeeds offline instead of failing
  slowly.
- **Status is what can be observed, and no more.** OpenCode publishes no
  language-server status and logs nothing when one fails to spawn, so a session
  records what its configuration named and whether the image had it —
  configured, unavailable, or disabled — and the header says that rather than
  inventing a lifecycle. Anything richer is an upstream contribution.

### The launch manifest

**Customization is a node-owned object, not a directory the harness discovers.**
Skills, standing instructions, agents, the approved plugin list, the language
servers and formatters turned on, the effective provider set and the policy
revision are built into one per-channel manifest with a content digest and a
revision number, and every session records the digest it launched under. The image a
session runs in is not part of it: that is the repository's, resolved at launch and
recorded on the session's start event with the repository image under it. A new
revision never changes a running session: the files were staged at launch, and
the next launch is what picks the change up. The node builds the manifest rather
than letting the harness resolve one because upstream's own resolution is not
safe to inherit — a duplicate skill name is resolved by overwriting one of them
nondeterministically, a `skills.urls` entry is fetched over plain HTTP before any
interaction with nothing that disables it, and a plugin resolves by a bare
existence check with no integrity verification. So duplicates are refused at
build, URL sources are refused outright, paths are validated and symlinks are
never followed, and a plugin name the harness *image* did not bake is refused
with the cache path it would have needed. Skill content is treated as code: a
`SKILL.md` body is also registered as a slash command whose template is
shell-interpolated, so an import records that plainly rather than implying the
package is inert data.

**Nested `AGENTS.md` and `CLAUDE.md` are read, and that is accepted.** OpenCode's
`read` tool attaches instruction files it encounters anywhere under the worktree,
with no flag that gates it — `OPENCODE_DISABLE_PROJECT_CONFIG` covers the project
config, the project's `.opencode/` directories and the top-level `AGENTS.md` used
for the system prompt, but not this. It matches what today's harnesses already
do: they read the repository, and the repository is allowed to contain
instructions about itself. The position that makes it safe is the one that has
always applied here — instruction content grants no permission. Every tool class
is `ask`, policy runs on the node, and a file in the worktree can ask for
something but cannot approve it. Nothing about the manifest, the orientation, or
the gate changes if a repository adds one.

**A settings file is not an instruction file.** Claude Code is launched with
`--setting-sources project` because that is what loads the repository's
`CLAUDE.md`, and the same source loads `.claude/settings.json`: permission rules
that allow a tool before the node hears the ask, and hooks and a status line that
run commands. So a workspace carrying one is refused at launch, as one carrying
`opencode.json` is for OpenCode, and the shipped policy refuses a write to any
`.claude/settings…` path or into `.git/` mid-session. Ignored files never reach a
workspace, so a checkout's own gitignored settings are unaffected.

**The ledger records a decision for every tool call, whichever harness asked.**
OpenCode asks the node before each call; Claude Code lets reads and searches through
on its own rules and never sends a `can_use_tool` for them. The Claude adapter notices
a call whose result arrives without an ask and reports it, normalized exactly as an
ask would have been, and the supervisor records the `policy_allowed` the node's policy
gives it, marked `decided_by: harness`. The call has already run, so nothing is asked;
if the policy would have asked or denied, the record says the harness let it through
and what the policy would have done, rather than naming a rule that did not apply.

### Model auth

**Model credentials are brokered like every other credential.** The harness holds
only a placeholder session token and reaches every provider through the node's
gateway, which injects the real credential by header — no TLS interception, the
harness's own request forwarded with its own shape, which is what subscription OAuth
tokens require. An Anthropic subscription is lent to Claude Code sessions only: the
gateway refuses it to any other harness rather than dressing that harness up as
Claude Code, and a session's harness follows its model's credential
(`node/src/adapter/compat.rs`) so the refusal is the exception, not the path. **The credential is lent for inference and nothing else**: each
provider shape carries an explicit allowlist of the methods and paths inference
uses, matched on a normalised path, so the account behind the key — organization,
billing, API-key and file endpoints on the very same host — is refused before the
credential is attached, and the refusal is recorded on the session that tried. The
node's own probe token is narrower still: the models list, and the one write it
makes, an embedding. The gateway is therefore the enforcement point for provider bindings
(fail closed) and the counting point for per-channel ceilings: usage is measured
where it happens, never reported by the harness. What a harness is wired to is the
same decision made early: only the providers that caller could actually spend on,
because a harness offers the whole model catalogue of every provider it is handed,
and a provider with no credential behind it fills the picker with models whose only
possible answer is a refusal.

**The node runs subscription sign-in itself.** Each subscription is minted by its
first-party client's OAuth app — Claude Code's for Anthropic, the Codex CLI's for
ChatGPT — and the node speaks those flows directly (`node/src/oauth/`): PKCE, a
loopback listener the node opens when the operator's browser is on its host, and
otherwise Anthropic's hosted code page for a paste or OpenAI's device code. Nothing
runs in a container and nothing is scraped from a terminal. The client ids and
endpoints were read out of Claude Code 2.1.247 and OpenCode 1.18.30 and confirmed
against the live services; they are the providers' to change.

Anthropic tokens are the refreshable kind asking only for `user:inference`: an
eight-hour access token and a refresh token that rotates on every use and expires on
a fixed date about a month after sign-in, so the operator signs in again roughly
monthly. A refresh revokes the access token it replaces at once. ChatGPT access
tokens last ten days, and their refresh token rotates too.

One sign-in serves the mesh. Signing in shares the credential, by default, with every
member bound to one of its channels, as a direct-sealed handoff. The credential carries
the sign-in it came from (a UUIDv7) and how many refreshes it has been through, and a
holder only ever replaces its copy with a newer one, so a delayed or replayed handoff
cannot undo a refresh. Because a refresh revokes every other holder's tokens, each
version is renewed once: any holder may try, but only after the hub grants it the
claim on that version (`POST /v0/claims`, keyed by a hash of the sign-in so the hub
learns nothing about it), and the winner hands the renewed copy to the rest. A node
with `[mesh] renew_credentials` tries half an hour before expiry and every other holder
a quarter of an hour, so an always-on node renews in practice and any holder covers for
it. Without a claim to be had — no hub, a hub older than claims, one that cannot be
reached — only the node that signed in, first in `nodes`, renews.

## The gate

### Privilege boundary

The gate only works if the harness cannot reach what the node holds. **Every node
establishes this boundary or does not run harnesses.** There is no advisory mode: a
node that cannot enforce must not quietly run credential-adjacent work with a label
on it. Refusal is the honest state, verified at startup, and a refusing node still
serves its interface and relays so the refusal is visible from anywhere.

The boundary is capability-driven, not tied to an OS or product. Two
implementations exist — rootless Podman (internal network with no DNS, a gateway
container carrying an allowlist CONNECT proxy and the node forward, the harness
container capability-dropped with the node holding its exec pipe) and Kubernetes
(one harness pod per session, a NetworkPolicy making the node the harness's only
route, the node serving the allowlist proxy itself). Both answer the same checks
behind the same seam. The harness reaches exactly two things: allowlisted provider
hosts through the proxy, and the node through the forward. On Podman a second forward
carries the node's per-client egress proxy, which opens nothing to a caller that
presents no grant; on Kubernetes a harness pod reaches the same proxy at the node's pod,
on `[gateway] egress_port`, which both policies must open before the node issues a
grant. The proxy never dials this machine or its link for a grant. In a cluster it sits
beside the API server and every service, so there it, and the allowlist proxy the node
serves there, also refuse private and carrier-grade NAT addresses,
`[runtime.kubernetes] internal_cidrs`, and cluster names (`.svc`, `.cluster.local`,
`.internal`, or no dot), before any lookup. Each looks a name up once, as absolute so no
search domain completes it, and dials only the addresses it checked, so a name that
answers differently a second time is never asked again.

Three things collapse the boundary and must be verified per environment:

1. **Runtime control in the harness.** A container-runtime socket or
   Docker-in-Docker gives the harness control of the gate.
2. **A shared privilege domain.** The harness must be unprivileged and isolated from
   the node; root in the node's own container fails the boundary even with no socket
   mounted.
3. **Direct egress.** Image build may use network; execution may not.

**Denied egress rejects; it does not drop.** A blackholed destination and a working
but slow one are the same thing to a caller, and the harness has callers that never
give up: OpenCode's language-server and npm installs pass no abort signal, and its
`edit` and `write` tools await the language-server touch inline, so one dropped
packet is an edit that never returns. Podman's internal network has no route out at
all, so a denied connection fails at `connect(2)` and a denied name fails at
resolution, both in milliseconds; the gateway refuses an unlisted host with a status
rather than silence. `--deep` measures this rather than assuming it, and fails a
boundary whose denials take longer than a few seconds. Kubernetes is the caveat: a
NetworkPolicy drops by nature and there is no portable way to make it reject, so
there the bound has to come from a cluster that can express one, and until it does
that runtime is honest about it rather than claiming the property.

**A harness container is a PID namespace with an init at its head.** Nothing in
OpenCode reaps what it starts — `serve` installs no signal handler, so a stop runs
no finalizer at all; its stop of a language server is a bare single-pid SIGTERM with
no escalation and no wait; and even a clean exit calls `process.exit()` before
pending finalizers run. So the runner stops the container rather than the process:
the init takes the signal, the stop timeout bounds the wait, and tearing the
namespace down is what actually collects the language servers and formatters. A pod
gets the same number as its termination grace period and needs no shared process
namespace, because the container carries its own init.

**A command run to completion in a pod reports what it did.** A check, a preparation or
a probe on Kubernetes is a pod of its own. Its exit code is its container's
`terminated.exitCode`, so a failing check (2) and a missing tool (127) stay apart; a pod
that ends without one (an eviction) is an error, never a made-up 1. Its output is the
pod log, where the cluster has already merged stdout and stderr, so it all arrives as
stdout and an empty stderr says nothing. Only the start is bounded by the runner
(`[runtime.kubernetes] start_timeout_secs`, 15 minutes by default, enough for a first
pull of a large image), and a start that runs out says what the pod was waiting for.
The run itself has no runner limit, as under Podman: the caller's own timeout stops it
by removing the pod, and a caller that stops waiting removes it too. The harness
image's entrypoint is named only when the pod runs the harness image; a repository
image runs the command as given. The identity a check's evidence is keyed on is the
image reference itself when it is pinned by digest, because the kubelet runs nothing
else under it, so an approval asked of a runner that has run nothing still finds it. A
tag is confirmed only by the `imageID` a run of that same tag reported.

**A harness pod is sized and given what a browser needs.** Every pod names its own
CPU and memory requests and a memory limit (`[runtime.kubernetes.resources]`), because
a namespace's LimitRange fills in whatever a pod leaves out and refuses a request above
its default limit at admission. A session and a command run to completion are sized
apart: by default a session asks for 250m and 1Gi and a check or preparation for 500m
and 2Gi, both limited to 4Gi of memory and neither to CPU, which suits one shared
4 vCPU / 8 GB node where a Laravel suite with Vite and Playwright peaks around 2 to
3 GiB. A limit below its request is refused when `node.toml` loads. Each pod gets a
memory-backed `/dev/shm` (`shm_size`, 1Gi), since a container runtime's 64 MiB crashes
Chromium, and an emptyDir at `/tmp` that `TMPDIR` names, so a repository image whose
own `/tmp` the harness uid cannot write still has scratch space. Neither emptyDir
carries anything the node did not put there, so the boundary's volume check holds.
`image_pull_secrets` names the Secrets a pod pulls with; left empty, the pod names
none and the namespace's default service account supplies its own, which is how a
registry integration that patches that account keeps working.

**The checks verify what runs.** They render the same run specification a session
uses onto a probe that is created but never started, so a mutating admission webhook
that adds privilege fails the check rather than passing the rendering, and a second
description of a session cannot drift from the first. `--deep` probes from inside:
no direct egress, denials that are refusals rather than silence, an allowlisted host
reachable, an unlisted host refused.

Contact-derived rules that stay rules: a bind mount over a symlink does not mask it,
so the harness state directory is built empty and only named files are mounted in —
nothing of the operator's leaks into a session, the same commitment as "nothing in
project repos" applied in the other direction. On a Podman machine the node forward
is TCP through the VM; on Linux it is a Unix socket and the gateway runs
`label=disable` so the harness never touches the socket while keeping its own
confinement.

### Credential classes

Two classes, and conflating them is the most likely way to build a gate that is
theater.

| Class | Held by | Notes |
|---|---|---|
| Identity / mesh keys | Node, generated locally, never transmitted | Enrollment via short-lived code from an enrolled node |
| Action credentials | Node broker, sealed, harness has no read path | Forge tokens, deploy creds, database credentials |
| Model auth | Same store, kinds `api_key` / `oauth` | Injected by the gateway; the harness holds a placeholder |

If action credentials sit on the same UID as the harness, the agent has Bash and can
read them. That is not hypothetical — it is the exact shape the broker was built to
fix.

Broker invariants:

- **No accessor returns a secret a response could carry.** The ways out are
  environment for a node-spawned subprocess, header injection inside the gateway
  module, and sealed handoff to another member. Listings carry names, kinds,
  bindings, and env key names only.
- **A credential binds to channels and to nodes.** Empty `channels` means no channel
  — unbound is unusable, not universal. Empty `nodes` means the node holding the
  file; a list pins further, so a copied store brokers nothing elsewhere.
- **The receiver's pin is the rule.** A handed-off credential not pinned to the
  receiver is dropped; the sender's bindings are a claim.
- **Widening a pin is explicit.** The CLI refuses to share a credential whose
  `nodes` does not already list the target. The authenticated interface may widen
  the pin — that is the operator deciding — and an empty pin gains the sharing node
  too, so sharing never locks the sharer out.
- **Forge tokens reach git through the environment only** — an inline credential
  helper fed by variables — never argv, never `.git/config`, never a remote URL, so
  neither `ps` nor the worktree the harness receives ever holds one.

### Brokered tools

What the agent needs are not CLIs to wrap but verbs the node exposes over MCP: the
credential never leaves the node, the harness never sees a process, and channel
bindings apply because the tool is the node's. Database access is a read-only SQL
runner guarded twice, on both sides of the privilege boundary.

**Consequential verbs are scoped grants, not open tools.** Merge, publish, ticket
transition, and deploy exist as narrow tools (`mr_merge`/`pr_merge`, `deploy`, a
ticket transition) rather than being absent, but each dispatch is decided twice:
the signed policy first — a trusted deny always wins — then a scoped [authority
grant](#authority-grants) naming that one target and, for merge, publish, and
deploy, the one revision it covers. A grant that does not exist, or whose target
moved, is asked, never assumed.

**Policy decides every tool call before the broker is touched.** An allow rule names
the tool exactly; a deny returns its reason to the agent; a tool the bundle does not
mention is put to the operator. Adding a tool never widens what runs unattended. A
missing, malformed, or badly signed bundle yields no rules, and no rules means every
request is asked: the failure mode of broken policy is more questions, never fewer.
Bundles are signed with a key never present on the hub, so a compromised hub can
serve stale policy but not new policy.

**Administration is an explicit operator capability, not a loopback exemption.**
Mesh admission, removal, and hub-key sharing, starting, joining and leaving a mesh,
and every `node.toml` edit accept authenticated administrators from the remote UI,
each confirmed there with a passkey (see [Reaching a node](#reaching-a-node)). Policy
signing/install and host service actions additionally require a local request (see [Reaching a node](#reaching-a-node)), which a proxy on
the node's own host does not make. Service actions name only the supported fixed supervisor,
refuse active work or ambiguous process identity, and report scheduling rather
than pretending a restart has already succeeded. The restart route that applies a
configuration change (see [Configuration](#configuration)) drains active work instead
of refusing it, and also restarts a node in a Kubernetes pod.

**A policy preview is a signed artifact with a fixed baseline.** Application
compares the preview's expected installed hash while holding the same policy lock
used for disk installation, cache replacement, and provenance recording. Refresh
cannot advance an old preview to a new baseline. Status reports verified installed
files separately from the actual running policy; a disk read cannot establish a
running hash. Initialization refuses any existing or partial signing installation.

**Sending a policy is not installing it.** Durable rollout targets become applied
only on an authenticated receipt matching sender, target, rollout, and exact
bundle hash. Offline, sent, and legacy targets remain unconfirmed; retry uses the
stored signed bytes. Optional application/wire/policy metadata is additive to
node advertisements without changing the wire contract. Missing metadata stays
unknown; neither a receipt nor an upgrade silently replaces a trust root.

### Authority grants

A grant is a local operator decision layered under signed policy, not a
replacement for it: `authority::decide` checks the signed bundle first (a trusted
deny dominates any grant, and an invalid or unsigned bundle still fails closed by
asking), then the store's live grants for that channel, action, and target — a
grant's own `deny` or `ask` outranks a broader policy `allow`, and no matching
grant asks. Every dispatch reads grants at that instant, so expiry and revocation
take effect immediately, never at the next poll. A grant is scoped to one target
(a pull request, a merge request, an issue's exact transition, a deployment job)
and, for merge, publish, and deploy, the one revision it covers; the target
moving to a later sha drops the grant rather than carrying it forward silently.
Grants are made and revoked one at a time in Settings → Permissions & policies.

One grant is not a side effect but a surface: **`terminal`**, an interactive PTY
inside one session's workspace. The harness's `POST /pty` spawns an arbitrary
command in an arbitrary directory with no permission check of its own, so the
grant is the only thing between a browser and a shell; it is therefore bound to
a session *and* to that session's workspace path (`terminal:<session>:<path>`),
requires a session binding to be an `allow` at all, and is refused if the
session is not active. With it, the gateway still rewrites rather than forwards:
the working directory is pinned to the workspace or a normalised subdirectory of
it, the caller's environment is reduced to variables that decide only how a
terminal *looks* (`PATH` and `HOME` come from the harness, never from the
request), and the command must be one of the shells the image itself lists at
`GET /pty/shells`. The WebSocket is reached with a ticket the node mints after
taking the harness's own server-side: bound to the operator, the session, the
PTY and the exact origin it will be presented from, for thirty seconds, once.
The proxy carries bytes both ways with a bounded queue in each direction and
closes with a stated reason rather than buffering for a client that has stopped
reading; input is never replayed across a reconnect, and the display is
reconstructed by the app rather than by tracon.

**A terminal grant is not a per-command ledger, and the node says so wherever it
is offered.** Once the shell is open, everything typed at its prompt runs with
no further decision and raises no tool call, because the harness raises none for
a PTY. What is recorded is the shape of the thing: that a terminal was opened,
with which shell, in which directory, under which grant, and how many bytes and
how long the connection lasted — not the transcript. Capturing a bounded tail of
the output to the session log is a deliberate configuration (`pty_capture_output`),
off by default, and labelled as what it is: a record of what the operator
happened to run, not of what tracon decided.

### Explaining authority

The question behind most low-value interruptions is not "may this call go
ahead" — it is "what is this session allowed to do at all, and why is *this* the
thing it has to ask me about". `GET /api/sessions/{id}/authority` answers it in
task terms: the node and its isolation state, the harness and image the session
actually runs under, what it can reach (workspace, egress allowlist, the
credentials bound to its channel), its limits, the grants live for it right
now, what runs unattended, and what it would still have to ask about.

**It is the gate answering about itself.** Every verdict in it is produced by
calling `Policy::explain`, which calls the same `decide` that answers the real
request, and by reading the same grants table `authority::decide` reads at
dispatch. A description maintained beside the enforcement would drift, and drift
is worse than silence here: the operator would be told what a parallel model
believes the gate does, at exactly the moment they cannot check. For the same
reason the unattended-command list is the bundle's own allow rules rather than a
summary of them, and each entry carries the rule id that produced it.

Three things it deliberately does not do. It does not collapse policy and grants
into one verdict — a grant can only narrow the gap between an ask and an allow,
never override a denial, so they are reported side by side. It does not report
an argument-scoped allow as an allow: `doc_write` is asked, *unless* the slug is
a working note, and saying either half alone is a lie the operator would act on.
And it grants nothing — the route is a read of state that already exists, and
nothing on it widens what a session may do unattended, least of all an external
write. Reducing interruptions is the operator's decision to make, with this in
front of them and Settings → Permissions & policies to make it in.

### Review

**Review the diff with a session that never saw the implementation.** A model that
watched itself reason toward a design will rationalize it; a fresh session given
only requirements and diff will not. The agent has no forge token and never runs the
publishing CLI: submitting captures a **candidate** — an immutable copy of the
exact Git tree at the submitted `head_sha`, addressed by hash rather than the
mutable worktree, kept even after the review moves on — and each file's blob hash
is recorded at submit so approval of a branch that moved is refused naming the
files. Required checks run against an isolated copy of the candidate, never the
worktree, in a throwaway container with no credentials, no egress, and only the tools of
the image the operator validated for that repository. Which commands and which image is
the repository's `[[repo]]` entry in the node's own configuration — never a file in the
candidate, which could then choose its own gate, and a table a running node takes in
live rather than waiting for a restart, so an entry saved from Settings or the CLI
applies to the next session and check without ending the ones in progress — over the
node-wide checks and the
harness image where the entry names none; a command that image has no tool for is
recorded `not_runnable` — never a pass, never reusable evidence, and never a failure
the agent is asked to fix. The checks belong to the node, not to the tool call that
asked for them: `submit_review` starts them in a task of its own and waits only as long
as a status call may (45 s), because an MCP client abandons a call that runs long —
Claude Code 2.1.247 ended them at 5–5½ minutes whatever its MCP config said, and no
client waits past the twenty-minute `timeout` the node writes there — and dropping
the call must not drop the check, its `check_run` row, or the
session's way back out of `waiting_on_check`. Past that the call returns `checking` with
the id the review will have, `review_status` carries the wait and then reports the
review or the same refusal the call would have given, and a retry of the same candidate
against the same check definitions attaches to the run instead of starting another. A
restart interrupts whatever was still `running` and closes the sessions that asked for
it, like every other live session. An entry may name the repository's Dockerfile instead of an
image: the node then builds it (`crate::repo_image`) from the **default branch**, read
through Git's object store and never from a working tree, adds one layer of its own,
and records the digest it built in its own state — the recipe is configuration, the
digest is not, because a locally built image has a different one on every node. That
is also why a recipe can be offered to another node (see Mesh frames) and its digest
never is. The
layer depends on who a run is (`repo_image::RunAs`). Under Podman it is root with no
capabilities in `/work`, `HOME=/root`, no entrypoint. A pod is never root: there the
layer makes `/work`, `/cache` and `harness_home` owned by `[runtime.kubernetes] uid`
(65532), sets `HOME` to that home, ends on `USER 65532:65532` and clears the
entrypoint, and the pod says `runAsNonRoot` with the same uid. That makes a contract the
node cannot enforce: what the repository's Dockerfile installs must be readable and
executable by any uid. The profile is part of the recipe's hash and of the session
layer's key (only where it is not Podman's, so Podman's existing digests keep their
keys), so an image built for one runtime is never used by the other. A check run builds first when the recipe has moved; an
approval only reads, so it cannot start a build, and a merged Dockerfile change is a
different execution image and therefore different evidence. Image builds are the one
place repository-authored commands run with the network and outside the boundary,
which is why the operator names each one. The build goes through the runtime's
`ImageBuilder`: Podman builds into its local store; on Kubernetes the node runs
`buildctl` against a BuildKit daemon over mutual TLS, sends the materialised context
itself, pushes to the configured registry, and takes the digest from the build's
metadata. The registry credential is mounted only in the node's pod, but BuildKit's
session auth hands it to the daemon on request during a build, so it is scoped to the
registry alone. `exists`, `identity` and `remove` are answered by a runtime-neutral
OCI distribution client (`boundary::registry`); a registry that cannot be asked keeps
the standing image in use, a builder that cannot build is never held against the
recipe, and a registry's superseded image is kept one generation longer for pods
still about to pull it. A session runs in the same image with the
harness layered on: a second image, `FROM` the repository's by digest with the harness
binary copied out of the harness image, keyed on both digests and probed before any
session is told to launch in it. On Kubernetes the builder is BuildKit, the harness is
copied from the release's harness image in its registry, and the layer also copies that
image's static `tini` to `/usr/libexec/tracon/tini`, which the session's pod runs as
`tini --` in front of the harness: a pod's first process is PID 1, which ignores
signals it does not handle and inherits orphans, and Kubernetes has nothing like
Podman's `--init`. The probe runs in a pod as the harness uid. Both harnesses are
layered. Claude Code is one binary and the environment its image sets. OpenCode is its
binary and the whole of `/opt/tracon`: the entrypoint that seeds its package caches (now
the layered image's `ENTRYPOINT`, and named in front of the harness in a pod), the
language servers and formatters its configuration names, a statically linked `rg` that
the entrypoint links into OpenCode's cache so it never downloads one, and its own Node at
`/opt/tracon/bin/node`. That Node is on no `PATH`: the toolchain profile names it in front
of each JavaScript tool, so the agent's shell keeps the repository's `node` and the tools
keep theirs, and a tool whose interpreter does not run on the image is reported
`unavailable` in the session's launch status rather than failing the session. Claude Code's layer now begins the same way OpenCode's does (`repo_image::layer_guard`).
It removes `/etc/claude-code`, the managed-settings directory Claude Code reads on Linux
whatever its environment says, whose policy outranks the node's flags, and any `.claude` or
`.claude.json` in the harness's home. It refuses `CLAUDE_*` and `ANTHROPIC_*` variables
other than the three every session sets for itself, as well as `NODE_OPTIONS` and the
loader and `BUN_*` variables below. The launch states `BUN_OPTIONS` empty, because Claude
Code's binary is Bun and a `--preload` there runs a script inside it. Its probe checks that
the binary, and a pod's init, resolve to themselves. The OpenCode layer
also removes what a repository's image could use to configure OpenCode under the node:
`/etc/opencode`, which OpenCode reads whatever its environment says, and anything already
at `/opt/tracon`, whose seed the entrypoint copies into the configuration directory. It
refuses, naming what it found, an image whose environment sets an `OPENCODE_*` or `BUN_*`
variable, `LD_PRELOAD`, `LD_AUDIT` or `LD_LIBRARY_PATH` (each would reach the harness
wherever the launch sets no value of its own), one with a non-empty `/etc/ld.so.preload`,
and one where `/opt`, `/usr`, `/usr/local`, `/usr/local/bin`, `/usr/libexec` or
`/usr/libexec/tracon` is a symbolic link that would send the copied files somewhere the image chose, such as the
workspace. The probe then checks that the entrypoint, the binary and the baked node each
resolve to themselves. The launch sets `BUN_BE_BUN=0`, because the binary is Bun and that
variable would make it run a `serve` script from the worktree with the server password in
its environment. The trust model is that the operator's image is trusted as its Dockerfile
is: it builds with the network and supplies every command the agent runs. So these checks
catch accidental configuration, not a hostile image; the boundary is the runtime around the
session. OpenCode is launched as `/usr/local/bin/opencode`, never by a `PATH` lookup. Checks stay on the repository's image, so a harness
upgrade rebuilds only the layer and no evidence moves. The session's start event
records the image it ran in, the repository image under it, and the reason when it ran
in the harness image instead. Dependencies arrive by **preparation**: the entry's `prepare`
commands run once per run, on a copy of the candidate, with a cache writable and an
egress grant for exactly the hosts the entry names, revoked again before any check
starts; every check then runs on its own copy of the prepared tree with the cache
read-only. That cache is the run's own, copied from the repository's base cache and
removed with the run. The base is written by one thing only — a preparation of the
default branch, made when the repository's image is built — so a candidate's install,
which may run the candidate's own scripts, can reach its own evidence and nobody else's.
That warm-up is also the only preparation lent a registry credential: the entry's
`prepare_credentials` name `env` credentials in the broker, each of which must name the
repository in its own `repos`, and their environment goes into the warm-up's commands
and is taken out of what they print. It travels as secret environment
(`RunnerCommand::secret_env`), as every egress grant's proxy URL does, since that URL
carries the grant's password. Neither is ever written where another reader finds it.
Podman is given each name as `--env NAME` with the value in the `podman` process's own
environment, which only the node's user can read, rather than on an argv any local user
can list. An env-file was the alternative and would leave the values on disk if the node
died before removing it. On Kubernetes, where a pod spec is readable by anyone with
`get pods`, each run gets an immutable Secret, `<pod>-env`, labelled with its pod. It is
created before the pod (a pod naming a missing Secret never starts), referenced with
`secretKeyRef`, and given the pod as its owner so the cluster removes it if the node dies.
It is deleted once the pod has started, because the kubelet has read it by then and a
pod that never restarts never reads it again. A run that ends before it starts (a
failure, a kill, a dropped caller) deletes it on the way out, and the boundary check and
`reconcile` sweep any a dead node left. The Secrets rights are optional in the `runtime`
check. A node without them refuses a run carrying secret environment and never
downgrades it to a plaintext value. A session is such a run everywhere: its token at the
gateway and the MCP server, OpenCode's per-launch server password and its egress grant
are secret environment, and Claude Code reads its MCP server, whose header carries the
token, from a staged file rather than its argv. A Kubernetes node without the rights
refuses every session up front, and issues no egress grant. A private package therefore reaches candidates and
sessions as cached bytes, never as a token, and a candidate that needs one the cache
does not hold fails its preparation rather than being lent the credential.
A preparation that fails runs no check: it is reported once, with the failing command,
its exit status and its output, each check it kept from running is recorded `not_run`
(never a pass, never reusable), and the refusal names preparation rather than a check.
Sessions get a cache of their own, one per repository and channel, that outlives each
session: it starts as a copy of the base, and what an agent fetches or compiles into it
(Cargo's output goes there through `CARGO_TARGET_DIR`) is there for the channel's next
session on the repository. No check mounts it, so nothing an agent wrote can make the
review gate pass; a check still prepares from the base. A session that needs
something running beside it (a browser to drive, a database to test against) asks for
it by name from the operator's `[[service]]` catalogue with `service_start`, which
waits, within a tool call's budget, for the entry's readiness probe; `service_status`
keeps waiting. It never names an image or a command. The service joins the session
container's network namespace, so it is reached on the session's loopback and reaches
the gateway and nothing of its own, and the policy bundle decides each service by its
name: the shipped bundle runs a headless browser unattended and asks about anything
else. Loopback is only the session's if the service binds nothing wider, so tracon
publishes the browser it means (`containers/browser`, Chrome started on 127.0.0.1
directly) and the `browser` workflow refuses any build that listens beyond loopback,
upstream base bumps included. The service's `service` events on the session are how every path that removes
the session's container (its end, a kill, a restart's reconciliation) finds and removes
its services first. A session's pause stops neither the harness container nor its
services today; both run until the session ends. A runtime that cannot share a
namespace (the local and Kubernetes runners) reports that it cannot run one rather than
starting it somewhere the session cannot reach. What a repository asks of
preparation and does not get is said before launch: `GET /api/preparation` (shown under
the repository in the composer) reads the checkout and names the install it would run,
the entry's `prepare` commands and the image, then every devcontainer field the node
will not honour (hooks, `initializeCommand` on the host, features, mounts, privileges,
environment, a `build`) and every install script a scripts-off install skips, each with
where that work belongs instead. Those that would stop preparation are told apart from
those it passes over, and none of them is ever run to find out: the preview reads files. A grant is one client's: the node serves its own
CONNECT proxy behind a second forward in the gateway, each preparation and session
presents the token it was issued as proxy credentials, and each is filtered
by its own host set — so nothing one may reach is reachable by another, none waits for
another, and a refusal is known to be that client's. Because that proxy dials from the
node's process, a granted name that resolves to the machine itself or to its link-local
range is refused whatever the grant says. A session, Claude Code or OpenCode, holds a
grant for its whole life: the hosts every harness may reach, plus its repository's `egress` where the
entry sets `session_egress`. A refusal under it is an ask, not an end: the node raises
an approval (`request_egress`, one per session and host) and answers the CONNECT with a
reason naming it, since a connection cannot be held open while the operator decides; the
refusal is recorded on the session once per host, and the orientation says the same
before the first tool call. The operator's answer widens that session's live grant — for
ten minutes, for its life, or for its life and the repository's `[[repo]]` entry — and
runs nothing; a declined host is not asked again by a retried connection. The grant
replaces the harness proxy in the harness's environment, which OpenCode's shell and
terminals inherit; the model and the MCP server are on the gateway host, which `NO_PROXY`
keeps off any proxy. What OpenCode fetches on its own (plugins, `rg`, language servers) is
still closed by its launch flags and its image's seeds, not by the proxy. A check's evidence is keyed on source
revision, check definition, execution image, dependency inputs, and the preparation and
egress it was given, and is reused rather than rerun only when every one of those is
unchanged — a resubmission that edits only the title,
description, or ticket prose reuses the same candidate's code evidence, and an explicit
rerun always makes a new record. Diff size is capped at submit, because complexity
accretes when nothing says no at submission time. An operator's hand-edit travels back
as a request for changes: the agent applies it and resubmits, and **the agent remains
the only writer to the worktree.** Every decision is recorded against the revision it
decided, with the requirements pinned as they were at submission — and bound to
it: approve, reject and request-changes all name the revision the operator
inspected, compared inside the statement that records the decision, locally and
for a verdict forwarded from another node. The commit cannot stand in for the
revision, because a resubmission may carry the same `head_sha` with different
requirements or prose; a verdict from a tab that read the revision it replaced
is refused and the review waits for a fresh reading. Unchanged code may reuse
its checks, never an unseen human decision.

Because every decision names its revision, a resubmission can show what answered
it. Beside the full diff against the base, the review carries the diff from the last
revision a verdict was given on to the one on the screen, and each piece of feedback
given on the way with the files the next revision changed. Both are read from Git in
the review's worktree between the two revisions' commits; when the worktree no longer
holds the earlier commit (a rewritten branch), the review says so rather than
approximating it, and a rebase between revisions shows what the base brought in too.

**An agent may draft a repository's entry; only the operator makes it one.** The
`repo_setup_*` tools read the default branch's commit, never a session's workspace, so
nothing a session wrote shapes the draft. A trial prepares that commit with a volume and
a cache of its own, reaching only the hosts the asking session can already reach, then
runs each check on a copy with the cache read-only and no network, the shape of a
required check. A host the draft names that the session cannot reach is reported as not
tried; asking for it with `request_egress` is how it becomes reachable, and hosts opened
that way are suggested in the next draft. A proposal is an approval whatever the policy
says, checked before the card is raised. The node writes it, as the operator left it, in
place of the repository's entry. It never changes whether that entry opens its egress to
sessions. Trials are held in memory: a restart drops a report, not a decision.

**The node vouches for the revision and the checks; what an agent shows is its own
account.** The candidate's tree, the required checks run against it and every decision
bound to it are records the node made. An attached demonstration — commands, output,
images — is the agent's or the operator's account, linked and hashed, never executed,
and flagged stale once it changes. It exists to help the operator judge a change, not to
settle it, and it stays with the review rather than going to the forge.

`show_work` is how an agent gives that account itself. Its files are read from the
workspace snapshot the node exports, never a host path the agent names: a path that
leaves the root, names `.git` or passes through a symbolic link is refused. They are
stored as an immutable HTML bundle (`shown-<id>`, a slug `doc_write` refuses), so a
page is served only from the preview origin under its content security policy; plain
files get a listing the node writes, and the Markdown account is rendered with its
images named rather than fetched. The `shown_work` row records the commit the
workspace was at; a review reads it against its latest revision and marks it stale
once they differ, beside the required checks the node ran on that commit. It is
node-local like those checks, attaches to the review the agent names or else to every
review of its session, and is dispatched before the policy is consulted, like the
operator interventions: it reaches the operator and nothing else.

**Narrative reports are not publication candidates.** `submit_report` creates an
owner/channel/session-bound report without Git or a repository. Acknowledgement
and requests for changes compare the inspected content hash atomically; mirrors
cannot change ownership, channel, session, or review kind. An acknowledged report
continues to replicate, while code publication and review-session creation reject
report IDs. Notification delivery records only push-service acceptance, never
human receipt; `report_status` exposes the separate operator decision.

**An issue draft reports back to whoever drafted it.** `report_issue` stores a
draft the operator publishes to GitHub or discards; publication records the
issue number beside its URL, and a discard records the operator's reason.
`issue_report_status` reads that outcome under the same ownership rule as
`review_status`: the drafting session, or the channel's external callers when
there was none, on the node that drafted it. Like `approval_status`, it is
dispatched before the policy is consulted, so a signed bundle that predates it
cannot turn reading one's own draft into a question for the operator.

**Nothing a session calls waits on a person past one status call.** A person
answers in minutes or hours and an MCP client abandons a call long before that, so
every tool that waits on the operator returns within 45 s with `still_waiting` and
a handle, and a status tool carries the wait: `ask_operator` returns a
`question_id` that `question_status` reads, `request_egress` and every gated call
an `approval_id` that `approval_status` reads, and `submit_review` a `review_id`
that `review_status` reads. The state lives in the store, not in the call, so a dropped or timed-out
call loses nothing and an answer given in between is read by the next status call.

**The node owns the wait, not the agent.** Nothing makes an agent call a status tool
again: it can end its turn on `still_waiting`, and a session whose turn ended used to
be finished with — a plan session closed `phase_done` on its plan, and the question
it was waiting on became unanswerable with it. So at the end of every turn, and on a
5-second tick between turns, the supervisor reads what the session waits on
(`session::waits`): its unanswered questions, its undecided reviews and narrative
reports (checks still running included), and the gated calls it asked the operator
to allow. A session with one open is not ended and its phase is not marked done: it
is held as `waiting_on_you`, harness and container kept, and the idle suspension of
published sessions passes it by. Each decision — answered, cancelled, approved,
changes requested, rejected, published, checks failed, run, expired — goes back to
it as its next prompt carrying what the status tool would return, together with
what is still open. Whatever a status call already returned decided counts as told
and is not sent twice. Once nothing is open and nothing untold remains, the turn's
end does what it always did; a closed work item still ends the session at once. A
decision the API makes nudges the supervisor; the tick is the backstop for one
nothing announced. Egress asks are not held on: a refused connection raises one
whether or not the agent wants the host. Nor are issue drafts, which say outright
that they do not pause the work.

A node that stops (or a restart that finds one) puts a held, idle session away as
`suspended` with what it was waiting on in its `state` event, rather than closing it
`node_restart`; its questions stay answerable. Once everything it waited on is
decided, the node carries it on as a new session on its workspace whose handoff
note carries each decision.
`question_status` follows `issue_report_status`'s ownership rule and is dispatched
before the policy is consulted for the same reason.

**Evidence reads preserve ownership.** Candidate lists and detail requests carry
their runner/owner and ordinary channel. Mesh handlers authenticate both channel
memberships, verify the candidate's actual channel and owner, refuse third-node
relays, and bound responses. Peer UI detail is read-only; it never turns a remote
ID into a local action. A peer's settings (its `node.toml` and its provider sign-ins)
are visible and editable only from nodes it names in its `[mesh] admin_peers`; each
change is confirmed on your node and checked again on the peer.

**What is reviewed is the tree; the commits and the branch are prose.** The
candidate is a tree hash recorded at capture, and every check, verdict and grant
is about that tree. How it reaches the forge is publication prose, like the
description: the agent proposes a commit message and a branch (`message`,
`branch` on submit; a session on the node's placeholder branch gets one named
from its title), the review lists the agent's own commits beside the diff, and
the operator edits either before approving. Under `commits = "squash"`, the
default, the publisher makes one commit holding exactly the reviewed tree, on
the commit pinned at submit — where the branch leaves its base for a new
branch, or what the change's branch holds for an update, including an earlier
squash the agent never had — with the candidate head's author and dates, so a
resumed attempt makes the identical commit. When the agent's own head already
is that commit but for its signature (one commit whose parents are that commit
and, for an update that merged its base, the base it merged; the message
unchanged), its head is pushed instead, so the signature survives; a commit
the publisher makes is never signed. The review says before approval which
of the two ships, and which signatures a new commit drops. `keep` pushes the agent's commits
as written, unless the operator edited a commit's message (`messages` in the
approved outputs, by the commit's sha): then that commit and those after it are
remade in the publisher with the same trees, authors, committers and dates and
their rewritten parents, so a resumed attempt makes the identical chain. A
commit the branch already holds is not reworded by an update. The message and branch are bound by the publish grant's prose hash
and target like the description, and a deterministic check (conventional type,
imperative subject, length, kebab branch, no tracker keys — each off unless the
repository's entry, the channel's `publish.style` binding or `[publish] style`
turns it on) refuses a submission that breaks it before the card reaches the
operator, and an approval whose edits do.

**An opened change is followed until it closes, and following only reads.** Every
two minutes the node reads each change it opened, using the brokered credential and the
same status call the forge tools make. That covers CI, draft or ready, open, merged or
closed, the review decision, and comments. It records what moved as a `forge_follow`
event on the submitting session, so the event lands on that session's work item too, and
pushes it named ("CI failed on `node`", "marked ready", "2 new comments"). The first
look is a baseline, not news. A change that has merged or closed is not read again.
Following never merges, approves, retries or comments. It polls rather than taking
webhooks, which would need the node reachable from the forge. A review with no
submitting session (a harness the operator runs) has nothing to hang the events on and
is not followed.

**A pipeline the agent started is followed for whoever started it.** A successful
`pipeline_run`, `pipeline_retry`, `job_play` or `deploy` subscribes its caller to the pipeline it started
(a `pipeline_follow` row, with what the pipeline looked like right after as the
baseline) and says so in its result. The same two-minute tick reads it through the
brokered credential: each job that passed, failed or was canceled is a `pipeline_follow`
event on the session (and so its work item), or on the channel's log for a harness the
operator runs, and the pipeline finishing, failing or stopping at a manual job is pushed
as well. Following ends there, or after a day without movement; playing the manual job
follows it again. It is a subscription of its own, so a pipeline outlives the merge
request whose merge triggered it. Following never retries, cancels or plays a job. An
agent that wants to see it through itself calls `pipeline_wait`, a read that holds up
to 45 s like `review_status` and returns as soon as the pipeline or a job moves, with a
`state` token to pass back so a change between calls is not missed. A GitHub Actions run
the agent reran (`run_rerun`) or started (`run_dispatch`) is followed the same way, under
the provider `github`: the rerun's result names the attempt it started, the run reads as
pending until GitHub has begun that attempt (so the last attempt's failure is not taken
for this one's), and `run_wait` is its `pipeline_wait`. GitHub names the run a dispatch
started; one that does not (an older Enterprise Server answering 204) leaves the node to
find it as the newest dispatched run of that workflow and branch that was not listed
before the call, and a dispatch whose run never appears is not followed.

**The two forges' tools are kept in step by a table, not by convention.**
`mcp::parity` maps each operation to its GitHub tool and its GitLab tool, or to the
reason one forge has none (GitHub Actions has no manual job to play, and no existing
deployment job to play at an exact SHA), and a test fails when a tool is added to
either forge without that decision. The names stay each forge's own (`pr_*` and `run_*`,
`mr_*` and `pipeline_*`), since those are the names agents know from `gh` and `glab`.
On GitHub the danger in starting CI is the workflow rather than the ref: a release runs
as a workflow on `main`. So `run_dispatch` starts only a workflow `[github] dispatch`
lists for the repository in `node.toml`, and refuses any other before the operator is
asked; refusing tags and inputs naming production, as `pipeline_run` does, is not enough
on its own.

**Publication is two side effects the node cannot take back, so it writes down
what it is about to do before it does it.** Approval imports the candidate into a
fresh publisher repository, pushes the reviewed commit (or its squash), reads the ref back from
the forge to confirm it is actually there, and then opens the change with
exactly the approved title and description — nothing is appended that the
operator did not see. Each step is recorded before it is attempted, keyed on the
review, revision, target and commit — so an attempt a crash interrupted is
resumed rather than repeated: the next attempt asks the forge what the branch
holds and whether a change from it, into the same base, at the reviewed commit,
with the approved text, already exists, and continues from what it observes. A
change at that commit whose text differs, or more than one that matches, is not
guessed between. A comment is recognised the same way, by its approved text.
A forge that cannot be reached, or an answer that does not settle which change
is this publication's, is recorded as `uncertain` rather than reported as
either success or failure. The review screen shows the latest record as one of
not attempted (refused before anything spoke to the forge), in progress,
failed, uncertain or published, with its remedy, and says before approval
whether a forge token is bound to the channel on this node — a binding, never
a claim that the token may push. The remedy is **Retry publication**, not a
second approval: it re-sends the title, body and outputs the record holds for
that revision, commit and target, and an uncertain attempt looks at the forge
before repeating anything. A newer revision, a moved branch, another target
or new prose needs a fresh approval, and binding a credential never retries
on its own. Every Git
command that can reach a forge — clone, fetch, ls-remote, push — is built in one
place with a brokered credential or none: an ambient host helper, an askpass
program, or an `ssh-agent` identity can never answer for the node, and there is
no setting that would let one.

What is published reads as the operator's own work. A session's commits are
authored and committed as the account behind the channel's bound forge token,
resolved once per token from the forge's `/user` (GitHub: the profile name and
`<id>+<login>@users.noreply.github.com`; GitLab: the name and the commit
address) and written into the harness's gitconfig; the workspace's own Git
config carries no identity. With no forge token bound, the host's global Git
identity is used, never a name for tracon. Forge API calls name only the HTTP
library as their user agent, and the node adds no trailers of its own, so a
harness's own (Claude Code's `Co-Authored-By`) pass through untouched. A
harness the operator runs is offered the identity by `tracon external show`,
and `submit_review` and the review screen name any commit not authored and
committed as that account. Rewriting one is never done silently.

## Workspaces

**Nothing is bind-mounted into the boundary.** A session's Git history is seeded
once, host-side, into a short-lived clone or worktree — a managed clone the node
refreshes with a brokered credential, or the operator's own checkout, which is
read and never written — and that seed is copied, never bind-mounted,
into a runtime-owned volume; the copy strips `.git` hooks, replace refs, grafts,
alternates, and `config.worktree` before anything node-side or harness-side reads
it. The harness sees `/work` and no host path. A workspace outlives its session:
resume it, export a checked snapshot, or download it. An export stages the tree
as Git sees it — the repository, and the tracked and untracked files its ignore
rules leave in — inside a container with no network and no capabilities (on
Kubernetes the node stages it from the claim itself, reading the index and the
ignore rules as files and running no Git), so the dependency trees and build
output a session leaves behind are neither the candidate nor a reason to refuse
the snapshot. Every copy keeps what Git records of a file: its bytes, whether it
is executable, and a symbolic link that resolves inside the tree, as a link. An
absolute link, or one that climbs out of the tree, refuses the copy. A workspace is
reclaimed only by an explicit sweep (`crate::gc`), once no unarchived session uses
it; a check run's volume is the one removed as soon as its run is over. Harness config is
materialized into scratch and passed explicitly, so instruction-file discovery by
directory walk stops mattering.

## Channels

A **channel** is the unit of separation: a key, not a row filter. A node not handed
the key cannot read the channel, and a meshed node refuses to start a session on a
channel it holds no key for. Channels carry bindings — which nodes may run them,
which provider, whether they notify, budgets and ceilings, which brokered tools —
enforced at the node, failing closed: a node asked to process a channel it is not
bound to refuses rather than falling back to a default. Bindings are recorded as
data, so "where did this content go" is a query rather than a recollection.

## The hub

A peer with a role, not a service tier: relay (routes ciphertext frames, keyed per
channel) and replica/processor (ordering, the nightly batch, for channels it was
explicitly handed). Both halves are one binary; the relay half never needs plaintext
and the work channel never gives it any.

**The hub is authoritative for ordering, never for availability.** Every node holds
a full local replica; reads resolve locally; sessions start with the hub
unreachable. Sync is hub-and-spoke so conflicts are only ever pairwise: site-stamped
writes, hub-assigned global order, HLC last-write-wins per record, tombstones for
deletes. Distributed SQLite options were evaluated and rejected — they all assume a
sync layer that can read the data, which the hub cannot for work channels.

**Trust asymmetry is resolved by key possession, not policy.** The cluster is
rented infrastructure, so the hub is the highest-value target and least-controlled
box at once. Channels the operator shares with it, it opens; a channel never shared
stays ciphertext. Opaque is the absence of a key, not a flag.

**Vectors are not a safe form of encrypted content.** Embedding inversion recovers
source text, so the vector index never replicates: each node embeds its own replica
and searches locally. A hub outage costs the other nodes' writes, not the ability to
search meaningfully. The index is derived state — delete it and it rebuilds.

### Mesh frames

The wire contract is `spec/README.md`, pinned by `proto/` with test vectors. A frame
is sealed then signed; every receiver verifies before opening, so a tampered frame
drops without the decrypt path running. The channel and epoch (or sender and
recipient) are bound into the AEAD associated data, so a re-labeled frame fails to
open. Direct sealing is how enrollment handoffs, credential handoffs, policy
bundles, and commands travel. Replay is closed twice (the hub remembers request
signatures, nodes remember frame ids); rekeying is a new epoch handed to the
still-trusted members, with old epochs kept so retained frames open.

Invariants on top of the wire:

- **A node speaks only for itself.** Mirrored rows naming another node than the
  verified sender are dropped and counted.
- **Receiver-owned presence.** `is_self`, `reachable`, and last-seen are set by the
  receiver, never believed from the sender.
- **Commands execute as local requests.** A command for a session or provider
  another node owns is sealed to the owner and runs there under the owner's own
  policy, store, and subprocesses; only the ack travels back. A provider sign-in's
  PKCE verifier, the code it exchanges, and the resulting credential never leave the
  owner.
- **The owner authorises the sender, not just the envelope.** Every member is on
  `@mesh`, so a verified sender proves only which node asked. Before a prompt,
  answer, kill, pause or resume runs, the owner checks that the session (or the
  permission's or approval's session or channel) is its own and that the sender and
  the owner are both members of that channel, by the owner's own record. It never
  relays a command on to a third node under its own identity. A provider sign-in,
  a pasted code and a sign-out are admin commands: the owner runs one only for a
  peer its own `[mesh] admin_peers` names, carrying an admin statement its
  operator confirmed for exactly that command on the owner (below). The pasted
  code that finishes a sign-in the sender started with a confirmed connect rides on
  that connect's statement (only that sign-in, from that sender, while it waits and
  for at most 15 minutes); a code for any other sign-in needs its own. Then the
  sign-in still needs the sender in every channel it would bind and in every
  channel served by the credential it would replace (one bound to no channel is the
  owner's alone and never replaced by a peer); a pasted code or a cancel is held to
  the channels of the sign-in the sender started. Create, verdict, notification, evidence
  and review-detail commands check their own channel as they run. A refusal is acked
  to the sender with its reason, logged, and shown as the mesh's last refusal.
- **A peer administers a node only with the operator's confirmation, made where
  the operator is.** An admin command from a peer (a provider sign-in, a read or
  edit of `node.toml`) runs only if the owner's `[mesh] admin_peers` names the
  sender. Every one but a read also carries an admin statement
  (`spec/README.md`): the sending node's operator confirmed exactly that command on
  exactly that owner with the sender's passkey (at the sender's own machine, by
  being there), and the sender signed what was confirmed, the line its prompt
  showed included, with its node key. The owner checks the signature against the
  sender's node id, that the statement names it and the command it is about to run,
  that it is no older than two minutes, and spends it once. The owner never sees the
  passkey and never trusts the sender's description of the command: the statement
  binds the command's own bytes. No peer changes who may reach or administer a node
  or where it sends its credentials (`[auth]`, `[mesh]` with `admin_peers`, a
  provider's upstream, credential and sign-in, the embedding endpoint and key).
- **A key handoff is taken only from a member.** A peer's `key_handoff` lands only
  for a channel the sender is a member of by the receiver's record (asked of the
  hub once more if the record lacks it, at most once a minute per sender), since a
  key from anyone else would add an epoch the receiver then seals the channel's
  frames under. It carries the channel's shared bindings; the ones a node keeps
  for itself (`external_stopped`, `external_paused`, `notify`) are never taken
  from it.
- **A command expires when its sender stops waiting.** A command sent with a
  timeout carries `not_after_ms`; past it (with 30 seconds' leeway for clock skew)
  the owner refuses it unrun, so a command the operator was told failed never runs
  later from the outbox. A prompt queued on purpose for an unreachable owner carries
  no deadline and is reported as queued, naming the owner, not as sent. A command
  from a sender on an older build names no deadline and runs as before.
- **A repository recipe is offered, never applied.** An operator may offer a
  `[[repo]]` entry's recipe to the other members of a channel it shares with them
  (`RecipeOffer`, sealed to each receiver like any command). The recipe names the
  repository by its forge identity (`github.com/owner/name`) and carries only the
  portable part of the entry: `dockerfile`, `context`, `checks`, `prepare`,
  `egress`, `session_egress` and `timeout_secs`. It never carries the entry's
  `image` (a digest names what one node built or chose), `prepare_credentials`,
  its `path`, or its `commits` and `style`. Both ends check it: the sender before
  it sends, and the receiver, which also refuses any field a recipe does not have
  and holds the sender to the named channel by its own record, as it does every
  command. Either refuses a command naming a home directory, an egress entry
  anything `inet_aton` would read as an address, a recipe over 64 KiB, and any
  command, egress entry, path or note carrying a
  control, format (bidirectional overrides, zero-width characters) or separator
  character, so what the operator reads is what runs. The receiver only stages
  the offer (`repo_recipe_offer`, node-local, never replicated) and announces it,
  one notification per sender and repository; nothing reaches `node.toml` until
  the operator, having seen what would change on the entry it lands on, accepts
  it. Accepting and offering are admin actions (a passkey from off the node), and
  the accept's prompt names the sender, the repository, the fields that change and
  what becomes of lent credentials. An accept names the review it was decided on
  (`viewed_hash`, over the target, the resulting entry and the operator's
  choices) and is refused if the table has moved since. The entry lands where it
  already answers for the identity exactly (or for this node's clone of it, or a
  checkout whose origin it is); a relative entry that only ends the same way is
  not chosen for the operator. Credentials the entry lends its preparation are
  dropped when the offer changes that preparation's commands, egress, Dockerfile
  or context, because a peer would otherwise choose what runs with them and where
  it may send them. The operator may keep them explicitly; the entry is then held
  (`repo_credential_hold`, on the preparation's commands and the credentials):
  `ensure_base`, and so every check, preparation, session and default-branch
  change, builds and warms nothing for it, using the image it last built, until
  the operator starts that build, itself an admin action bound to the hold it
  names, whose prompt names the entry, the peer whose preparation it is and what
  is lent. A later offer onto a held entry that leaves its preparation as it is
  keeps it held ("Still held"); one that changes it drops the credentials unless
  kept again. An operator who rewrites the held entry's `prepare` or its
  credentials at the node has decided what runs with what, and the hold lapses;
  narrowing its egress or changing its Dockerfile does not. A write that fails
  leaves the hold as it was. The accept prompt leads with what becomes of lent
  credentials, and a peer's name is shown without control or format characters
  and bounded. Accepting goes through the one `[[repo]]` writer, so the entry
  keeps its comments and every field the recipe does not carry, and the node then
  builds and pins its own image from the recipe. A newer offer from the same
  sender for the same repository supersedes an undecided one; a sender holds at
  most 32 undecided offers on a node and makes at most 10 an hour; an offer is
  staged for at least an hour and lapses after 14 days, and is withdrawn once its
  sender no longer shares its channel. Offers not taken up (superseded,
  declined, lapsed, withdrawn) are forgotten after a day, accepted ones after 30.
- **The hello may carry state, not secrets.** A node's models and its provider
  summary (names, states, identities — and a pending login URL) ride the hello,
  sealed to the operator's own mesh. Credential values never do; those move only as
  direct-sealed handoffs.
- One contract version, checked at enrollment. A peer on an older build drops
  payloads it cannot read; senders surface that as "unreachable, or on an older
  build" rather than silence, and as "runs an older build" when the peer's last
  hello named an earlier release. From this build on, a node that cannot parse a
  command still opens it far enough to ack it with "the owner runs an older build",
  so the sender hears why at once instead of timing out.

### Owner streams

A command is one message and one ack, retried until it lands. The native
harness API is not that shape: it is a request with a body, a response with a
body, and — for the event stream and the terminal — no end at all. Replaying
those through the durable outbox would repeat mutations and fill the hub with
bytes nobody reads again. So a request on one node for a session another node
owns opens a **stream**: its own frame family, relayed by the hub and stored
nowhere (`spec/README.md`, "Owner streams").

- **The owner decides, from its own state.** On an open, the owner re-checks
  that the session is its own, that the stream is on that session's channel,
  and that the opener still holds the channel *by the owner's own record* —
  then runs the request through its own gateway, matrix, policy and intent row.
  An operator the serving node admitted is refused here if this node grants
  them nothing.
- **Input is never replayed.** A stream that dies is gone. The serving node
  re-opens on the next request; it never re-sends a body, and a request with a
  body is refused rather than retried when the owner's epoch has moved. A
  mutation whose answer was lost is `uncertain` on the owner (the intent row
  written before dispatch) and says so on the response head.
- **Owner fencing is an epoch, not a heartbeat.** Each run of a node mints one;
  the serving side remembers it per session and names it on the next open. A
  restart fences every stream held against the run that died.
- **The hub sees ciphertext and routing metadata.** Channel, sender, recipient,
  stream id, epoch and sequence are clear because they are what it routes on;
  the body is sealed under a key derived per stream *and per direction* from
  the channel epoch key, with a label no durable operation uses. The hub bounds
  what it carries — per stream, per connection, per member, per minute — and a
  stream it has to drop is closed at both ends with a reason, in the clear,
  because it cannot seal a close of its own.

## Memory and documents

The node owns memory; harness-native memory is disabled. **Bank identity comes from
the channel and project id in the mesh, never from cwd or git root** — ephemeral
worktrees at varying paths are exactly the case filesystem-anchored memory
fragments on.

Two axes: scope (global, client, project, session) and kind. Directives are
human-only and always injected; facts inject at high confidence; lessons inject only
after promotion through the approval queue, batched nightly — curation is the
difference between a memory system and a landfill, and it is the cheapest reuse of
the gate. Hard token cap on injection: oversized context degrades the session it was
meant to help.

Retrieval is FTS first — the highest-value lookups are exact — with vectors fused in
where a node is given an embedding endpoint, and the vector contribution bounded
below one tier step, because "directives above facts" is a decision about whose
instructions win, not a relevance heuristic. The embedder is a named endpoint, not a
linked-in model: that is what lets a work channel point at a local server and leave
nothing, while another channel goes through the gateway where its bindings and
ceiling still apply. Pin the embedding model and record it on every vector row.

Documents are the same store, separate table, different lifecycle: long, named,
edited as artifacts, chunked on headings for retrieval but always returned with
their slug. Memories point at documents — a twenty-token pointer beats an injected
page. Session orientation is assembled per session from the corpus, the node's own
facts, and the policy's deny reasons, delivered as a read-only file and recorded as
an event, so the transcript shows what the agent was told.

**Orientation separates tracon's account of itself from the operator's.** The
system orientation is generated from the node's own state: the node and harness,
the item and phase, the tool names this channel serves, the enforced agreements.
It is followed by sections that carry a heading saying they are not tracon's —
*Operator notes*, the standing text on the channel's launch manifest; *Selected
context*, the documents the operator chose for this work item; and *Pinned
documents*, the ones the operator pinned to every session on the channel. A
session that cannot tell a system fact from a human's preference weighs neither
correctly, so the boundary is stated rather than left to be inferred.

**Orientation also reserves its space for the work before the guides.** What the
session is for and what bounds it — the node it runs on, the item and phase, the
plan or the diff under review, the enforced agreements, the operator's directives,
notes and launch customization, the item's selected context and the channel's
pinned documents — is assembled first and is never dropped to make room for
anything else. Other ready work is discretionary and gets what is left of the
context cap, in ledger order.
The cap used to be enforced by truncating
the assembled text, which cut from the end, where the task and the constraints were:
long guides could leave a session a wall of advice and no job. Whatever does not
fit is named — which piece, how much is missing, and the call that fetches the rest —
in the orientation the agent reads and on the event the operator reads. A bare
`trimmed` flag is not enough for either of them: neither can tell from it whether the
thing they needed was one of the things that was left out.

## Work ledger

Beads-inspired, not Beads: ready-work as a deterministic topological order every
replica computes identically, hash ids that two nodes mint offline without
collision, and a `discovered-from` edge so work found mid-session survives the
session. Storage is the node's SQLite, replicated per channel; readiness is derived,
never stored.

**Work items are an opt-in structure, not a requirement.** A plain prompt starts
an execute session claiming no item, plan, or review lifecycle at all. Naming an
item is what turns structural enforcement on: the `plan` phase requires one,
picking an item for `execute` gates on its plan artifact existing (a channel
binding can waive that), and closing the item ends the session that held it.
Context rot is mitigated by mechanism where the workflow opts in, not by a line
in a markdown file a plain session never claimed to follow.

**Coming back to work is one view, item or not.** An item's attempts are the
sessions that held it. A plain session's attempts are its lineage, meaning what it
continued and what continued it, so it gets the same view without being made into
an item. The view (`/api/work/{id}/continuation`, `/api/sessions/{id}/continuation`)
gathers what the work was for (the item, or the first prompt), each attempt and how
it ended, what the operator decided (answered permissions and questions, and the plan
and brief), what is in the way, the workspace the latest attempt left, and the
reviews and shown work. It also gives the one next action. Every part of that is
read from recorded state, and the next action follows fixed rules over those records,
so it cannot claim more than they say. It offers three verbs. Continue carries the
work on from the last ended attempt's workspace, in that attempt's phase, except
that a plan session which wrote its plan is carried on by executing it. Change
approach does the same and hands the operator's new direction, in their own words,
to the next attempt.
Abandon closes the item, or stops a plain lineage and puts it away, and records the
reason. Abandon deletes nothing.

### The product brief

An item may point at one document that says what the work is for: intended user,
problem, source references, constraints, success criteria, open questions. It is
an extension of the item and the corpus, not a second store — the brief is a
document of kind `brief`, the link is one nullable column on the item, and the
evidence it rests on is referenced, never copied. An item without a brief is not
an incomplete item; no phase asks for one.

**Every line says who is behind it, and the node enforces the part an agent
cannot be trusted to keep about itself.** A line is `observed` (the customer said
or did this), `inferred` (someone reasoned to it), `decided` (the operator chose
it), or, written by hand with no marker, `unattributed` — which stays
unattributed rather than being promoted to one of the three. A session may not
record a decision, because deciding is the operator's, and may not record an
observation citing nothing, because an observation no one can check is an
inference; a line a session does write carries a reference to that session, in
the document. The shipped agreements name `brief_read` and do not name
`brief_note`, so reading is free and every line an agent would add is asked.

**Lines point at evidence rather than holding it.** A reference is
`[doc:slug]`, `[session:id]`, `[evidence:id]`, `[work:id]`, `[url:…]` or
`[file:path]`, and reading a brief resolves what this node can: a reference it
has never seen is marked unknown, the way an unknown dependency is, instead of
being rendered as a link that goes nowhere. A bracket in an unknown shape is
prose, not a broken link.

The document is the record. It is Markdown that round-trips: the structure is a
reading of the file, not a schema the file depends on, so a brief can be written
or edited by hand with no node running, and a section or a heading this node does
not recognize is kept and written back rather than dropped for not fitting. A
section with no lines says what it does not say — an empty success-criteria
section reads as "no success criteria stated" — because a brief that has never
heard from the customer should look like one rather than like a tidy form.

### Criteria bound to what would settle them

A success criterion is not a new record: it is a line in the brief's success
criteria section, and what points at it is an indented line under it naming a
`check`. The document stays the record, so links travel with the channel,
round-trip, and are editable by hand. A check is the one link kind anything
produces a result for; whatever a check cannot settle is the operator's
judgement, recorded as a verdict rather than a link. The `scenario` and
`observation` kinds an older brief may hold are retired: they are still read,
so the file round-trips and they are not mistaken for criteria of their own,
shown as retired, counted for nothing, and refused for anything new.

**A criterion is named by its own text.** `sc-` and twelve hex characters of
the SHA-256 hash of the line with only outer whitespace trimmed. Case,
punctuation, comparison operators and internal whitespace are significant:
changing the requirement requires a fresh judgement, never an inherited one.
Reordering does not change identity. Earlier verdict rows are retained; on the
same brief, a changed criterion leaves its prior verdict visible as orphaned.
Switching briefs does not transfer a verdict even when the text matches.
Identical lines remain separate display rows, both marked as duplicates; neither
can be linked or judged until the brief distinguishes them.

**An agent may propose what good means; the operator decides it.** A link
carries the same provenance as every other brief line and is checked by the same
rule, so a session's link is `inferred` and a `decided` one is refused. Coverage
follows: a criterion whose only links are somebody's proposal reads as
`only_proposed`, not as covered, however green that proposal would go. A `check`
resolves only against the commands the operator configured — any other is
recorded and reported as never running.

**Passing one's own checks is not the customer agreeing.** A check result raises
a criterion as far as `checks_pass`, shown as "checks pass · unjudged".
The word *met* comes from append-only, node-local `criterion_judgement` rows
written only by the operator; review sessions may read criteria, but no MCP tool
settles one. An explicit candidate must belong to the item's channel and be
associated by its owner session or a pinned review revision. When provided, a
revision must pin that same item and candidate. An omitted or null candidate
on a judgement write means no attempt, even if a new candidate was captured
after the screen loaded; candidate-specific verdicts name the attempt the
operator saw. Reads with no candidate still default to the newest attempt.
An earlier verdict does not settle a later candidate. Index-based link removal
requires the displayed brief hash: another editor's change refuses a stale
index instead of deleting a different link.

Beside the criteria the view carries the gaps: what nothing points at, what
nobody has judged, every inferred or unattributed line in the brief, the
questions it still asks, and the orphaned verdicts — because a coverage view
that lists only what is covered reads as though that were everything there is.

### Selected context

The brief says what the work is for; it is not everything an attempt should start
from. The research behind it, the decisions already taken, the constraints the
answer must keep and the documents that matter to this item and not to the whole
channel are chosen by the operator, per item, and **follow every subsequent
attempt** — plan, execute, review, and the revised attempt after a request for
changes. Channel-wide pinned documents and memory recall are not a substitute:
one is chosen for every session on the channel, the other for whatever the query
happened to match.

The selection is a document of kind `context`, `context-<item>`, beside the
item's `brief-` and `plan-`: one line per document, `- [doc:slug] why it is
here`, under a heading naming its role — Brief, Research, Decisions, Constraints
or Documents. It travels with the channel and is readable and editable by hand
with no node running; prose written into it that is not a selection line is
kept. It is the operator's: `doc_write` refuses a `context-` slug outright,
before the policy gate would put it to the operator as an ordinary edit, because
a session choosing its own context is the substitution the selection exists to
prevent. A session that thinks a document belongs asks.

**What each attempt received is recorded, not inferred from the selection.** At
launch the node resolves each selected document against what it holds and places
it in the orientation in full, in role order, within an allowance of its own
(48,000 characters) that nothing discretionary can spend. A document not on this
node, archived, an HTML bundle, or past the allowance is not dropped silently: it
is named in the orientation with the call that fetches it, and marked on the
receipt. The receipt — one per session, never rewritten — records every selected
document's role, slug, hash and delivery: whole, cut short, or left out and why.
Its digest is over what was received, not who received it, and the item's
context **revision** counts distinct digests, so two attempts that started from
the same bytes share a revision and the first to start from something new mints
the next. Each receipt also names what changed since the previous attempt at the
item — a document added, removed, moved between roles, edited since, or
delivered differently — and the orientation says so in one line ("Context
revision 3 for this item. Since the previous attempt (revision 2): `ref-traces`
edited since the previous attempt"). An item whose selection was removed after an
attempt used it still gets a receipt, so the removal is visible rather than
indistinguishable from never having had one. Receipts are node-local, like a
launch manifest's revisions; the exact text an attempt was given is also on its
orientation event.

## Sessions, phases, budgets

Plan, execute, and review are **separate sessions the node spawns**, not phases
inside one — which sidesteps subagent model inheritance entirely, since nothing
is spawned as a subagent of another session's model. A session names its own
model, or the channel's phase binding, the channel's own binding, or the node's
model catalogue fills it in, in that order — resolved once at spawn and recorded
as the session's `model_source`, so a silent default is never undocumented.
Only an explicit model unusable for the channel's bound provider is a validation
failure at spawn; an empty one never is.

**What a session needs depends on what it is for.** Investigating, verifying and
publishing ask different things of a repository, and `GET /api/readiness` (the line
under the composer) says what each lacks before a session is spent finding out. An
investigation needs a node that would start it and a checkout to start it in, and
nothing else: no checks, no forge credential, no brief. Verifying adds required
checks and an image they can run in, since without them nothing the node runs can
vouch for the result. Publishing adds an `origin` on a forge the node publishes to
and that forge's credential bound to the channel; a work item with no brief is noted
there, since its review will have no requirements to judge against, but not held
against it. The answer only reads: it builds no image, calls no forge and starts no
session.

Budgets are denominated in tokens (dollars are derived where a provider binding
carries a price) and enforced by killing the session, checked at turn end because
that is when harnesses report usage — a property of the protocol stated honestly in
the interface rather than papered over. Channels carry daily ceilings enforced at
two points: session start is refused, and the gateway refuses the model calls of
sessions already running, so a running session stops spending and the operator
decides.

**An exhausted provider is a policy, not a failure.** The gateway reads every refused
model call and says what kind it was on the session's `provider_error`: throttling
and outages clear on their own and the harness retries them, an auth failure does
not clear by waiting, and exhaustion (a spent quota, a subscription's usage window,
an empty balance) is the one a channel chooses for, with a per-run override:
`pause` (the default), `fallback` to a named model, or `fallback_then_wait`. The
decision is recorded (`provider_exhausted`: policy, reason, model, next wake) and
the session is fenced like any pause. Its safe boundary is the fenced turn settling,
and nothing resumes or continues before that boundary is recorded. A waiting
session is woken by the node's periodic tick once the provider's own reset time has
passed. The reset is only ever one the provider sent, so a provider that sent none
leaves the session for the operator, because a guessed timer either wakes it into
the same refusal or sleeps past the lift. Before it goes back to work, everything that
would refuse it starting is checked again: the channel, the node, the harness pin,
the model's binding and credential, the ceiling and the budget. If any check fails,
the session is held with the reason. A fallback is a continuation from the boundary,
on the fallback model, with the policy carried over, because a running harness
cannot change model. A continuation already on its fallback has nowhere further to
go.

**A cache read is recorded, never charged.** Each step of a long session resends its
whole context, most of it from the provider's cache; charged as input, a planning
pass on this repository cost 3–4M against a 2M default budget for about 50K tokens of
new work, so the budget measured context length rather than spend. Both sides charge
the same quantity — fresh input, cache writes and output — whatever the provider's
dialect: Anthropic names its cache beside `input_tokens`, OpenAI counts it inside and
says how much, and the gateway brings both to one meaning before it records the call.

There are two sources for that usage and they are kept side by side, per turn, in
one ledger. The gateway counts every model call on the wire and that count is what
budgets and ceilings are charged; the harness reports its own numbers and those are
display until the two agree. At turn end they are reconciled: agreement within
tolerance is recorded, disagreement writes a `usage_mismatch` event carrying both
numbers, and the harness's figure may raise the charge but never lower it. A harness
that saw no usage reports zero rather than "unknown" — so does the gateway, if the
provider returned none — and a turn where calls went out and nothing could be counted
is marked `unmetered` rather than charged zero. Unmetered is not free: it is an event,
a flag on the turn, and a figure the channel's ceiling reports beside the day's
counted spend, because a meter that has silently stopped is worse than one reading high.
A turn the session ends under — stopped, killed, its harness gone — will never be
reported by the harness, so it is settled `interrupted` from the gateway's count
alone; it used to stay open and cost nothing.

Anything checkable deterministically is checked deterministically, between phases,
in a container with no credentials. Model supervision is reserved for judgment with
no test: a cheap model watching an expensive one mostly pays twice to learn what
the test suite would have reported.

**What a session came to is read, not written.** Its outcome record
(`GET /api/sessions/{id}/outcome`, the Outcome panel on the session) answers five
questions from rows other paths recorded: what changed (its reviews, their files
and line counts, how often the workspace moved), what the node verified (every
check it ran for the session, marked current only on the commit the latest review
stands at), what needs a decision (open permissions, unanswered questions,
undecided reviews and reports), what is uncertain (an unsettled dispatch, unmetered
or mismatched turns, a check still running or only run on an earlier commit, stale
shown work, a publication that may or may not have reached the forge), and the cost
from the turn ledger. The agent's own words appear only as claims, each beside what
backs it, and the only thing that backs one is a passing check the node ran on the
commit the claim was made at. A report has no commit, so nothing backs it; a review
that describes a new commit as green is unbacked until a check runs there. However
well the summary reads, prose never becomes verification.

## Metrics

Event log, append-only, replicated. Human time is claim/release on approval items —
a browser heartbeat measures tab focus, not attention. Durations are recorded
monotonically on the node that observed them; never subtract timestamps across
machines. The numbers that matter at six months: approvals per accepted change,
tokens per accepted change. Provenance — which model, which prompts, which approval,
which policy version shipped a commit — is a query over data captured for metrics
anyway.

## Clients

Every node serves the same embedded SPA; a client is a matter of shell.

- **No session state in the shell.** Sessions live in the node; a client crash is a
  reconnect, never lost work. Unsent prompt drafts are held by the node per session:
  saved on a debounce, handed back to whichever screen next opens the session, cleared
  by dispatching the prompt and by nothing else, and never delivered to the harness on
  their own. A review's unsent reason and edited prose are held the same way
  (`/api/reviews/{id}/draft`), with the revision they were written against, until a
  verdict is recorded; each save names the version it was made on, and a save on any
  other is refused with what is there, so two devices are shown each other's draft
  instead of one overwriting the other. The one exception is an in-progress diff edit,
  confined to the desktop browser's local storage — the surface least likely to be
  evicted.
- **The phone is not a node.** No replica, no keys at rest. It reaches a node
  directly over HTTPS with a session cookie; the hub never talks to a browser. A
  backgrounded PWA cannot hold a socket, so notifications are Web Push — sealed to
  the phone's own key, sent by every node the phone subscribed at, deduplicated by
  tag. Pushes are hints; the queue is the truth.
- The service worker caches the shell and never `/api`: a cached queue you cannot
  act on is worse than an honest "cannot reach the node".
- The desktop app is a tray client and an installer — it holds no session state and
  runs no node itself. It puts the node under the user's service manager, installs
  the CLI the unit runs, and after an update moves both onto the version it carries
  once no session is running. A node an earlier version spawned as its child is
  stopped once, before the service takes the port.
- **Two windows, and only one of them is privileged.** The main window is the node's
  own interface, and it holds the commands that install the CLI, install updates and
  restart the node — commands the node itself must never have, because a session
  inside the boundary would then hold them too. OpenCode's native UI is third-party
  code driven by an agent, so it gets a second window and its own capability, and
  that capability grants nothing: no `remote` section, so the origin loaded there can
  invoke no command at all, of this app's or of any plugin's. The window is created
  on demand on a boot URL the node mints, never at launch; it may navigate only to
  the node's OpenCode UI origin, which is a distinct origin from the node's own —
  same-origin with the operator API is a page that drives the node with no credential
  — and the app refuses a boot URL that would collapse the two. An `http(s)` link off
  that origin is handed to the system browser by the app's own navigation handler, in
  Rust, so reaching the browser is not a capability the window holds; any other scheme
  is refused. The boot token rides in the URL fragment, which is not sent in requests
  and is stripped from anything the app logs. Nothing about this depends on the page
  behaving; it is the window's grants and its navigation handler, both asserted by
  test against the manifest.
- **A browser gets the same isolation without a window to put it in.** The desktop
  app can hand OpenCode's UI a window with its own capability; a phone cannot, and an
  installed web app that opened the system browser would have given the session away
  to a context it no longer controls. So the interface hosts the native view itself,
  at `/sessions/{id}/opencode` — a route inside the app's own `scope`, holding a
  cross-origin frame on the node's OpenCode UI origin. The isolation is that origin,
  exactly as it is for the desktop window: no `sandbox` (a sandboxed frame's origin
  is opaque, which that listener refuses outright), no message bridge, nothing
  delegated, and a status bar read from tracon's own session API rather than from the
  frame. Both policies name the other: the interface's `frame-src` names the UI
  origin, the UI origin's `frame-ancestors` names the interface.
  Being framed changes one thing on the node. The cookie the view's own bootstrap
  sets is third-party from the browser's point of view, and third-party cookies are
  blocked by default on phones — so the bootstrap says whether it is framed, and a
  framed exchange is answered with a `Partitioned` cookie (CHIPS) instead of
  `SameSite=Strict`. That keys the cookie to tracon's own site as the page around it,
  which is stricter than what it replaces rather than looser: the origin guard already
  required a matching `Origin` on everything that writes.
- The interface talks only to the node that served it; that node mirrors peers and
  forwards commands to owners. A verdict executes on the owner, because staleness
  and publishing need the owner's worktree and broker. A mirrored review's checks,
  evidence, criteria, staleness and forge intent are read from the owner when it is
  opened, bounded like candidate evidence. The owner answers only for a review it holds,
  on a channel both nodes share. An owner that cannot be read, or that has moved on to
  a revision not yet mirrored, is said to be, rather than shown as an empty review.
- **The attention count is actionable human decisions, and nothing else.** Everything
  parked in the operator's bay is sorted into three lanes before it is counted: what a
  person can decide now; what the agent holds (a review being revised, a request that
  lapsed into its default denial); and what something outside this node holds (a
  publication in flight, an outcome to reconcile, a node that has to come back first).
  Only the first lane is counted, in the rail, on the home and per node, from one
  classification so those three can never disagree. The other two keep their threads,
  each stating why it is not yours: a smaller number earned by hiding work would be
  the same defect in a new place.
- **A permission card carries the intent of the session that asked.** The node joins
  each waiting request to its session and work item — item, phase, channel, branch,
  the session's own state — and the card links into both. Answering "may I run this"
  by first opening the session to find out what it was for is most of the cost of the
  interruption. Expiry is shown as what it is: once a request lapses the card offers
  no answer, because the node has already denied it and recording a deferral as
  consent is the one thing the gate must not do.

## Reaching a node

Three surfaces, three answers, and the differences are the point.

- **The harness** gets a separate listener with a per-session bearer token and no
  operator API: a harness that finds the forward cannot drive sessions.
- **The operator on the node's own machine** is the operator by definition: a
  local request is answered with no credential. Local is one test
  (`http::auth::is_local`) behind the guard, the `Loopback` gate and every
  `local` flag the interface reads, and it asks more than the peer address. A
  reverse proxy on the same host (`tailscale serve`, cloudflared, a pod's
  sidecar) connects from `127.0.0.1` too, and a peer-only test made every caller
  behind it the operator, cookie or not, on the routes that rewrite `node.toml`.
  So a request is local only when the TCP peer is loopback, it carries no
  forwarding header (`Forwarded`, `X-Forwarded-*`, `X-Real-IP`, `Via`, and the
  CDN, Envoy and Tailscale client headers), its `Host` and any absolute target
  name `localhost`, `127.0.0.0/8` or `::1`, and its `Origin`, if any, is an
  HTTP(S) loopback origin with the same host and port as that `Host`. A page on
  another loopback origin (the OpenCode UI listener, a dev server on another
  port) is another site: it can send a simple cross-origin POST without a
  preflight, so it must not count as the operator. The Host and Origin checks
  are also the DNS-rebinding defence. The `Host` port is not compared with the
  bound port, so `kubectl port-forward` to another local port and the Vite dev
  server stay local; a proxy that rewrites `Host` presents the bound port
  anyway. There is no list of trusted proxies. What the test cannot see is a
  same-host relay that strips forwarding headers and rewrites `Host` to its
  upstream, which is by construction a request from the machine; proxies in
  front of a node must send `X-Forwarded-For`. Nor can it see a layer-4 relay
  of the operator port (`tailscale serve --tcp`, `socat`, `ssh -R`, `kubectl
  port-forward --address 0.0.0.0`): it adds no header and a non-browser client
  behind it chooses its own `Host`, so it is local. Don't relay the operator
  port that way.
- **`[auth] public_origin`** is the name the node answers to from elsewhere:
  an `https:` origin on a domain name (plain `http:` only for `localhost`),
  normalised at load. An IP address is refused, loopback included, because a
  passkey's relying-party ID must be a domain and browsers refuse WebAuthn on
  an IP-literal origin. It is not a credential and does not widen what is local. The push
  subject falls back to it when `[notify] contact` is unset, and it pins the
  passkeys' origin and relying-party ID. Unset, every passkey route and every
  remote admin action refuses (`step_up_unavailable`, `no_public_origin`): the
  node will not guess which origin its passkeys belong to.
- **The operator from anywhere else** needs the token, exchanged for an HttpOnly,
  Secure, SameSite=Lax cookie — a cookie because `EventSource` cannot set headers,
  Lax because a push notification's deep link is a top-level cross-site navigation.
  Only hashes of the token and of each cookie are stored, so reading the database
  mints nothing. Reissuing the token revokes every client in the same transaction.
  Secure means HTTPS is a requirement, not a recommendation, and the interface says
  so rather than looping. A login link may carry the token in the URL fragment —
  never the query string — because fragments are not sent in requests and the app
  strips them before anything renders.
- **An admin action from anywhere else also needs a passkey** (`http::passkey`).
  The cookie is enough to use the node, not to change who may use it or what it
  lends: setting or removing the operator token, authority grants, credential
  import, share and forge tokens, provider keys, sign-ins and channels, channel
  bindings, boundary setup, mesh invitation, admission, removal and hub sharing,
  reopening a channel to external harnesses, offering a repository recipe to
  peers and accepting one into `[[repo]]`, and passkeys themselves. So are
  the approval answers that change the node rather than let one call run:
  allowing a repository setup proposal (it writes the `[[repo]]` entry),
  allowing a Dockerfile trial (a networked build outside the boundary), and
  allowing an egress host for the repository. A peer cannot answer one of
  those over the mesh; the gate is per node. Most peer commands are authorised
  by channel membership; the admin ones (a peer's provider sign-ins and its
  `node.toml`) need the peer to name the sender in its `[mesh] admin_peers`
  and a statement the sender's operator confirmed with the sender's passkey
  (see [Mesh frames](#mesh-frames)). Each handler takes an `AdminAction` extractor, which
  reads the body once and hashes the request it is about to run:
  `op_hash = sha256("tracon-admin-op/1" | method | path and query | If-Match |
  sha256(body))`. Without a proof for exactly that hash the request is refused
  with a typed `step_up_required` carrying an assertion challenge bound to it;
  the interface asks for the passkey, posts the assertion, gets back a proof, and
  sends the identical bytes again with the proof in `X-Tracon-Step-Up`. Challenge
  and proof each live two minutes, are spent once by a conditional `UPDATE`, and
  belong to the cookie session that asked: a proof is useless with another
  cookie, on another request, or a second time. The proof is stored as a hash.
  A bearer token never confirms anything off the machine, because a
  confirmation belongs to a cookie session and a bearer request has none; the
  remote CLI's admin commands are refused with that reason. A local request
  passes without a passkey: the node's own machine, including `kubectl
  port-forward`, is the recovery root. The verifier is `webauthn-rs` (OpenSSL
  built from source, so the static musl build stays static): attestation
  `none`, user verification required, a discoverable credential preferred and
  no attachment pinned, so a synced phone passkey, a hybrid phone across the
  room and a security key all work. Every `node.toml` write is one of these
  admin actions too, including the whole-file `PATCH /api/config`, where the
  prompt lists the keys the save changes (see [Editing from
  elsewhere](#editing-from-elsewhere)). What remains loopback-only acts on the
  host rather than on configuration: skill import from a host directory, policy
  signing and installation, the service lifecycle, the storage sweep and the
  boundary re-check.
- **Pairing enrolls the first passkey.** Spending the token stamps the session's
  `paired_ms`; for fifteen minutes after, and only while no passkey is
  enrolled, that session may enroll one without confirming. The insert itself
  is conditional on the table being empty, so of two freshly paired clients one
  wins. After that, adding a passkey is an admin action, so a token that leaks
  later (a photographed QR) buys a cookie but not admin. Rotating the token
  ends every session and every ceremony in flight and keeps the passkeys, so it
  never reopens the window. Removing a passkey cancels every outstanding
  ceremony and proof. Losing every passkey is recovered at the node: remove
  them there and pair again (see `docs/RECOVERY.md`).
- **A window without passkeys borrows a browser's** (`http::device`, shaped like
  RFC 8628 device authorization). The desktop app against a remote node is the
  case: its webview has no WebAuthn on Linux (webkit2gtk) or in an unsigned
  macOS build. To sign in, the window posts `/api/auth/device` (public; ten
  asks per address per five minutes, four waiting per address and sixteen in
  all) and gets a secret `device_code`, an eight-consonant `user_code`, and
  `approve_url = <public_origin>/#approve=<id>`, which it opens in the system
  browser or shows as a QR. The ID rides in the fragment; the secret is in no
  URL and is stored as a hash. A browser signed in to the node opens the
  address and shows what the asker claims to be, its address and when it
  asked (for a step-up, also the asking session's short ID and what approved
  that session), but not the code: the operator types the code the asking
  window shows, compared in constant time, and the third wrong one denies the
  request. That is what ties an approval to the window in front of the
  operator rather than to a link someone sent them (RFC 8628 §5.4). Approving
  is itself an admin action, bound to that request alone: `sha256(
  "tracon-device-approval/1" | id | user_code | kind | op_hash)`, so off the
  machine it takes a passkey assertion for it, and no cookie (including the
  asking window's own) approves anything by itself; at the node it needs none.
  The window polls `/api/auth/device/token` and is answered
  `authorization_pending`, `slow_down` (its interval raised five seconds),
  `access_denied`, `expired_token` (ten minutes) or `invalid_grant`, until it
  collects, once, a new session cookie scoped like a login's. The code is
  looked up before any limit applies, and only polls whose code answers to
  nothing meet the per-address and global poll limits, so junk cannot block a
  window that holds a real one. That session
  records `delegated_from` (the approving session's short ID, or `local`),
  lives 30 days rather than 90, never has `paired_ms`, so it cannot enroll the
  first passkey, and its sign-in is pushed to the operator's devices. For an
  admin action, a window refused with `step_up_required` posts its challenge
  to `/api/auth/device/stepup` instead of asserting. Only a challenge the node
  opened when it refused a request carries that request's description
  (`webauthn_challenge.action`, `.summary`), and only such a challenge becomes
  an approval: one asked for by operation hash alone, with no refused request
  on record, is refused, and the window sends the request again for a fresh
  refusal. Nor can approving a device request be delegated in turn: its
  challenge (and any fresh one for the same operation) is marked
  not-delegable, so the approving browser's own passkey answers it. Every
  agent- or client-supplied word that reaches a prompt or the approval page
  (a host, repository, Dockerfile, user agent) is stripped of control, format
  (bidirectional, zero-width) and separator characters and bounded, and an
  answer that edits an approval's arguments names the edited keys first in
  its summary ("Edited host: Save …"). The approval takes the operation, request and summary the node
  recorded, is bound to the asking session (three waiting at most), and the
  poll (accepted only from that session) collects a proof for the original
  `op_hash`, with the same single-use, two-minute and session rules as one
  from an assertion. A bearer token neither approves nor asks for a step-up.
  Rotating the token or removing a passkey revokes every approval not yet
  collected; rows stay a day after they settle as the record of who approved
  what, and the log keeps each decision. The address an approval shows and
  the limits key on is the TCP peer, or, when that peer is loopback or in
  `[auth] trusted_proxies` (an ingress controller's pod network, say), the
  rightmost `X-Forwarded-For` entry that is not itself a trusted proxy. It
  only tells callers apart; it never makes a request local.

## Constraints

- **Single user.** No tenancy in the auth model; one human holds keys. Channels are
  cryptographic separation of one operator's contexts. Recorded so nobody, the
  author in a year included, retrofits multi-user onto it.
- **No business domain.** Clients, invoicing, and time billing live elsewhere.
- **Bootstrap.** The node is developed by agents running inside it, so a documented
  path to running a harness outside the system is maintained, and rebuilding the
  node is a host-side recipe, the CLI's or the desktop app's. Restarting it is also
  an administrator's call on the node's HTTP API (`POST /api/admin/restart`), so a
  node in a pod or reached from a phone can apply its own configuration; it is never
  a tool, and it needs an administrator (at the node, or confirmed with a passkey),
  so no session inside the boundary performs it. A harness outside
  the boundary runs as the operator and could; see
  `reference/external-harness-notes.md`.

## Data lifecycle

Encrypted snapshots of the hub's volume to object storage, with a restore path that
has been exercised — hub failure without a tested restore costs years of context.
Retention is decided (2026-09-29): the node keeps everything, and the operator deletes
by hand. Settings → Data shows what the node holds per kind (rows counted from the
store, directories measured on disk), where each kind that has a real delete is
deleted, and what that delete does beyond the node; the Nodes screen carries the
serving node's total. Documents, memories and work items delete one at a time and
replicate the delete as a tombstone; workspaces and harness state go through Runtime
storage once their session is archived; sessions, their logs and evidence have no
delete. Tombstone semantics wait until replication makes a
propagated delete matter, and that decision is due before it does. Plain-text export for every kind: no format readable
only by this binary. Documents export as plain Markdown and import back by filename
alone; a session package is JSON that `tracon session show` renders with nothing
running; the vector index is derived and is rebuilt rather than restored.
`RECOVERY.md` is the operator's copy of this, including the way out through a
harness run directly and what that costs.

A harness session's own state is a separate lifecycle with its own three verbs,
because the harness database carries no version and an older build opening a newer
one writes to it silently. **Backup** refuses a live session unless `--quiesce`
stops the harness through the supervisor, then checkpoints WAL, `VACUUM INTO`s a
copy it verifies read-only, archives the rest of the state tree minus caches, and
records the workspace's commit and dirty digest at the same instant. **Upgrade**
never touches the original: it migrates a clone under the target build, refuses
outright when that build is not on the host, and swaps the clone in only if its
integrity check passes. **Restore** is bounded by what it can honestly put back —
it refuses a build or a state-schema generation this node's runtime does not cover,
refuses while the session runs or its state is claimed, and brings back the harness
database and state tree but never the workspace or any side effect the session had
outside its own state, which it says before it acts.

## Open questions

1. **Tombstone semantics.** Retention is keep-and-delete-by-hand (see Data lifecycle);
   how a delete propagates across replicas is undecided and waits on the mesh work.
2. **Stacked MR handling.** Whether the node owns restacking or feature flags on
   trunk make stacks unnecessary. The trunk setup favors flags.

## What rots and what does not

The parts of this system that will rot are the harness adapters and anything
tracking harness features. The parts that keep paying are the credential broker, the
brokered tools, the review gate, the corpus, and the ledger. Build so that when the
orchestration layer becomes obsolete, the corpus and the ledger outlive it.
