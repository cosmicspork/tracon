# Plugins and other MCP servers

Tools a session is offered beyond the node's own: another MCP server the node
proxies, or a plugin the node installs, supervises and brokers. Writing a plugin
is [the plugin contract](../reference/plugins.md).

## Other MCP servers

An MCP server outside the node (a tracker's, a design tool's) is registered as an
`[[mcp_server]]` under **Settings → Configuration** or in `node.toml`: its
streamable HTTP address, the channels it serves, and optionally a broker credential
whose `TOKEN` is sent with each request. Only the node speaks to the server. An
execute session, or a harness you run yourself, sees each of its tools on the node's
own MCP endpoint as `<name>__<tool>`, deferred behind tool search like the node's
other tools. Each call is decided before the node makes it: a signed policy denial
refuses it; the entry's `deny` list refuses it, its `ask` list puts it to you, and
its `allow` list runs it unattended (only while the policy bundle is valid). A tool
no list names is asked, unless a policy rule allows `<name>__<tool>` by name. An
asked call is an approval like any other, holding the exact arguments the session
sent, and runs once you allow it. A server that cannot be reached is reported to
the caller, not put to you. `GET /api/mcp-servers` lists each server, whether it
could be listed, and how a call to each tool is answered now. Only streamable HTTP
is supported; a stdio server is not.

## Plugins

A plugin is a program, optionally with a static UI, that a repository publishes on
GitHub in a fixed shape, and that the node installs on your approval. These are the
node's own, not the harness plugins a channel's launch manifest installs into a
session. Each GitHub release carries one `<name>-<version>-<target>.tar.gz` per target
(`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`) and a
`checksums.txt` that lists them. An archive holds `tracon-plugin.toml`, `bin/`, and
optionally `ui/`, and nothing else. The manifest (contract 1) gives the plugin's name,
version, the program it runs under `bin/`, the broker credentials it may be lent, the
tools it offers, its UI, the hosts it says it reaches, the document formats it owns, and
the node methods it calls (see "A plugin calling the node" below).
An unknown key is refused.
Writing a plugin is covered in [docs/reference/plugins.md](../reference/plugins.md):
the manifest's fields, the release shape, the process the node starts, the signed caller
context, previews and formats, and testing against a scratch node.

`tracon plugin add owner/repo` fetches the latest release for this node's target and
checks the archive against `checksums.txt`. It refuses an owner outside `[plugins]
owners` (`cosmicspork` by default) before asking GitHub anything. It also refuses an
archive whose entries are not plain files and directories, and a manifest that does not
validate or does not match the archive's name. Then it stages the build under
`<state dir>/plugins/bin/<name>/<version>-<digest>/` and raises an **install card** on
the channel you name with `--channel` (or `[session] default_channel`). The card lists
the tools, the credentials (marking those lent only to a call from an approved card),
the UI, the egress it declares (shown, not enforced) and the formats, and pins the
archive's sha256. Nothing is installed until you allow it. Allowing it is an admin
action, so from off the node's machine a passkey confirms it. Before the build becomes
the active one, the node checks it again on disk. `tracon plugin add ./dist/<archive>.tar.gz` stages an archive
you hold, for testing the shipped shape on a scratch node. It is pinned by its own
digest and held to no owner, since you are its source. A plugin that declares a format
another plugin owns is refused, naming the owner. So is one named like an
`[[mcp_server]]`, since their tools share the `<name>__<tool>` names.

`tracon plugin update [name]` raises a card for a newer release, showing what changed in
the manifest and the release's notes, while the installed version stays active.
`tracon plugin rollback <name>` goes back to the version before, which was approved when
it was installed, so no card is raised. `tracon plugin rm <name>` removes its builds and
keeps its data under `<state dir>/plugins/data/<name>/`; `--purge` deletes that too.
Rolling back and removing are admin actions, as is restarting a plugin's process.
Settings › Plugins can do the first two as well: each plugin's row has **Check for
update**, which raises the same update card on the channel picked in the card's header,
and **Roll back to `<previous>`**, which asks in the page first. That works from the
PWA on a node you reach through the hub, with a passkey confirming the rollback.
Installing and removing stay with `tracon plugin`.
`tracon plugin ls` lists what is installed, the version before it, and any build waiting
on a card. It also lists each tool of the active build and how a call to it is answered
now (runs, asked or refused) and by which rule. That answer is the signed policy's on a
channel no rule is scoped to, with the plugin's `[[plugin]]` entry applied. Settings ›
Plugins shows the same.

The node runs a plugin as one process per channel, started when a session on a channel
it is offered to first lists its tools. A process stops once it has gone unused for its
`[[plugin]]` entry's `idle_timeout_secs` (15 minutes by default). It also stops when the
plugin is updated, rolled back or removed, and when the node shuts down; the next use
starts the version active then. The node checks the build's digest again and runs its
`exec` with `--socket <path> --data <dir>` appended (and `--node-socket <path>` for a
plugin that calls the node). The environment is empty except for
`TRACON_PLUGIN_TOKEN` (fresh for each start and held only in the node's memory),
`TRACON_NODE_KEY` (the node id) and `TRACON_CONTRACT=1`. Credentials are never in it:
they are lent per call. The socket is in a 0700 directory of the node's runtime
directory (`$XDG_RUNTIME_DIR/tracon/p/`, or `/tmp/tracon-<uid>/tracon/p/` where that is
unset), and the plugin's data and working directory is
`<state dir>/plugins/data/<name>/<channel>/`, also 0700. A process is ready once MCP
`initialize` answers on its socket within 30 seconds and its `tools/list` names exactly
the tools its manifest declares. A plugin that lists a tool it does not declare, or
leaves out one it does, is unhealthy at once with the difference as its last error
(`its tools disagree with its manifest: lists x not declared; declares y not listed`),
offers nothing, and is not started again for that build until you restart, update or
roll it back. One that exits or is not ready is
started again after a wait that doubles from 1 second to 60; if it fails again after the
60-second wait it is marked unhealthy and left down until it is next used or restarted.
Its output is kept in memory and in `<state dir>/plugins/logs/<name>/<channel>.log`,
rotated at 1 MiB with three kept. `tracon plugin ls` shows each process's state, pid,
uptime, restarts and last error, `tracon plugin logs <name> --channel <c>` its last
lines, and `tracon plugin restart <name> --channel <c>` starts it afresh, or starts it
on a channel where it has not run yet. Settings › Plugins shows the same. A `when_idle`
node restart waits for running plugin processes as it waits for turns.

A plugin's tools are offered to execute sessions and external harnesses as
`<plugin>__<tool>`, beside the node's own, on a channel that binds a credential the
manifest declares (by its `name`, or a binding its `pattern` matches), or on every
channel when it declares none. A plan session is offered only jira-review's reads
(`search`, `issue`, `transitions`), and a review session none. A call is
decided as an `[[mcp_server]]` tool's is: the signed policy first, under the `tool` kind
and the namespaced name, then the lists in the plugin's `[[plugin]]` entry, by the
plugin's own name for the tool. A signed denial wins, then the entry's `deny`, then its
`ask`; its `allow` runs a tool without asking only while the signed bundle is trusted. A
tool that neither names is asked. The manifest's `read`/`write` hint is shown on the card
and never decides anything. A tool the manifest marks `consequential = true` is never
run because something allows it by name, neither the bundle nor the entry's `allow`: the
node adds a required `operation_id` to its schema (at least 8 ASCII letters, digits or
dashes and nothing else, `^[A-Za-z0-9-]{8,}$`, the same rule as `pr_merge`, `mr_merge` and `deploy`;
kept by the node and never sent to the plugin), asks every call, and runs one
unattended only under a `plugin_call` [grant](../reference/configuration.md#granting-authority) naming that exact
call, which its card shows. An allowed call is recorded under its `operation_id`, so
the same id and arguments again return the recorded outcome rather than running twice.
The entry's `deny` and `ask` still apply:

```toml
[[plugin]]
name = "sql-review"
allow = ["query", "describe"]   # by the plugin's own tool names; `*` globs
ask = ["explain"]
deny = ["drop_*"]
timeout_secs = 120              # one call; 0 or unset is 60
idle_timeout_secs = 900
# require_preview = false       # allow its cards without a shown preview (default true)
```

Each call carries a caller context in `params._meta.tracon`: the plugin and tool, a hash
of the arguments, who is calling (an agent, its session and harness, or `external`), the
channel, the approval it was allowed on if any, a 30-second expiry, a nonce, and the
sha256 of the instance's `TRACON_PLUGIN_TOKEN`, all signed with the node's identity key
(the `TRACON_NODE_KEY` it was started with). A plugin refuses a context for another
process, another call, a stale or replayed one, or one the node did not sign. The wire
format is in sql-review's README, "Caller context wire format, v1". Arguments must have a
canonical form, so a float is refused before it is asked: pass decimals as strings.

The same context lends the call, in `credentials`, each credential the channel binds that
the running build declares, as its environment fields. One the manifest marks
`lend = "approved"` is lent only to a call run from a card you approved, never to one the
policy or the entry allowed outright. An undeclared credential is never lent, and none
is ever in the plugin's environment. The card for a plugin's tool shows the version the
call was asked on, its hint, the hosts it says it reaches, and what an approved run is
lent. An approved call runs only on that build: if the plugin was updated or rolled back
while the card waited, the card says so and allowing it runs nothing, and the same call
asked again is a new card on the build active then. A call holds its
process for as long as it runs, so the idle stop counts from the end of the last call.

When the card for a plugin's tool opens, the node asks the plugin what the call would
do (`tracon/preview`), with a context made as for an asked call: no approval, and
nothing marked `lend = "approved"`. The card shows the preview's heading and its body
(a diff, a table, or the statement, as Markdown), marked "not previewable" when the
plugin cannot show the effect. A plugin that has no previews gets the generic card. One
that refuses is shown refusing.

Allow needs a preview by default. It is refused unless the card was shown the plugin's
preview of the call exactly as it would run, your edit included; a "not previewable"
answer counts as shown. A plugin that has no previews, or whose preview refuses, fails or
times out, can't be allowed until you opt it out with `require_preview = false` in its
`[[plugin]]` entry. The card disables Allow and names that setting. Reject and Request
changes always stay open. The node enforces this when it records the answer, so an answer
from the API, a peer or the queue's inline card is held to the same rule; one that showed
no preview is told to open the card, and a push for such a card opens it. Calls the
policy or the entry allow outright never make a card, so they are unaffected, as are
node tools and `[[mcp_server]]` tools. The preview's token, the
version it was taken against, stays with the node; the page is shown only its hash,
and your answer sends that hash back, so an answer read from an older preview (another
tab opened since) decides nothing. An approved call carries the token as
`preview_token` in its signed context. The plugin applies against it and, if what it
names has moved since, refuses with `data.stale` and sends nothing. The approval then
fails as stale (not uncertain), and `approval_status` says `stale: true`, so the agent
calls again for a fresh card. When you edit the call, the page asks the plugin to preview
the edit (`POST /api/approvals/{id}/preview`) once you pause. That preview's token replaces
the kept one, and Allow waits until the shown preview is of the call as edited.

A tool's string argument may be marked `"x-tracon-format": "<format>"` in its input
schema, naming `markdown` or a format a plugin owns (manifest `formats`). Marked
arguments are the card's prose; marked ones inside an array's items make it the card's
list of items. Prose in a plugin's format is shown as that plugin renders it to Markdown
(`tracon/format/convert`, a pure function lent nothing), an edit is rendered again as
you type, and the source is one click away. If the owner is removed or cannot answer,
the card shows the source. The contract for previews, tokens and formats is in
[docs/reference/plugins.md](../reference/plugins.md#previews-and-formats).

### Plugin pages

A plugin whose manifest has `[ui]` (an `entry` under `ui/` and a `title`) gets a page in
the interface's **Plugins** section. Opening it frames the plugin's `ui/` files, which
the node serves itself, on a loopback port of the plugin's own: the first free one in
`[plugins] ui_port_first..ui_port_last` (7440 to 7459 by default), kept for the plugin
so its origin stays the same. The frame's address carries a capability token minted for
that plugin and channel; the page's scripts run on the plugin's origin, never the
interface's, and its CSP lets them talk only to that origin. What the page asks of
`api/...` reaches the plugin's process for the channel as your request: the node passes
the method, path, body and a few headers, and adds a `tracon-context` header holding the
same signed caller context a tool call carries, with kind `operator`, tool `tracon/ui`,
and `arguments_sha256` the hash of `{"body_sha256", "method", "path"}` for that request.
A credential with `select = "profile"` lends such a request only the binding its
`?profile=` query parameter names, or the bare one without it, and one lent only on
approval is never lent to a page.
A page's changes must come from its own origin, and they are logged to the channel's
external log under `/plugins/<name>/ui`; reads are not logged. `deny = ["ui"]` in the
plugin's `[[plugin]]` entry (or a signed policy rule denying `<name>__ui`) turns its
API off; its files are still served. Pages are opened on the node's own machine only:
the listener is loopback, and reaching it from the phone through the hub comes later.
Every URL a plugin's page uses must be relative (Vite's `base: './'`, `fetch('api/x')`),
since it is served under `/t/<token>/`.

### A plugin calling the node

A plugin can call the node back, as itself, for what it cannot do alone: put a card in
front of you, push you a notification, or use another plugin's tool. It names the
methods it calls in its manifest, and the install card lists them under **Calls the
node**:

```toml
node_methods = ["approval/request", "approval/status", "notify", "tools/call"]
```

A plugin that names any is started with `--node-socket <path>` as well: a socket the node
serves for that one start, in the same 0700 directory as the plugin's own, gone when the
start ends. Each request is a JSON-RPC 2.0 call, `POST /` with a JSON body and
`Authorization: Bearer <TRACON_PLUGIN_TOKEN>`. Any other token, an earlier start's
included, is refused with HTTP 401 (`-32001`), and a method the manifest does not name
with `-32601`. The node knows which plugin and channel a request is from by the socket it
arrived on, never by what the request says. Errors: `-32602` for params the method does
not take, `-32003` for a refusal (`data.decision` is `denied` or `asked` for a tool call),
`-32000` for a failure.

- **`approval/request`** puts a card in your queue: `title` (one line, up to 200
  characters), an optional Markdown `summary`, and `content`, a list of 1 to 64 blocks
  (512 KiB in all), each in a format the node renders: `{format: "markdown", text}`,
  `{format: "diff", text}` (a unified diff), `{format: "word_diff", before, after}` (a
  text before and after, which the page diffs by word itself: words taken out struck
  through, words put in highlighted, both texts a click away), `{format: "table", rows:
  [{key, value}]}` and `{format: "source", text, language?}`, each with an optional
  `title`. `request_key` makes a repeat while the card waits return that card (otherwise
  identical content does), and `expires_secs` shortens its wait. It returns
  `{approval_id, state: "pending", new}`. The card names the plugin as who asked. A plugin
  raising one while it serves an agent's call passes that call's `_meta.tracon` as
  `on_behalf`: once the node has checked the signature is its own, for this instance and
  channel, issued in the last ten minutes, and from an agent, the card is that agent's
  session's as well, so the session waits on it and hears your decision. A card takes no
  edit: allow it, reject it, or ask for changes.
- **`approval/status`** takes `approval_id` (or `approval_ids`) of cards this plugin
  raised on this channel and waits up to `wait_secs` (at most 45, the default) for a
  decision, as an agent's status call does: `still_waiting`, `approved`, `rejected`,
  `changes_requested` (with your `notes`) or `expired`. Allowing a card runs nothing on
  the node; the plugin does what it asked to once it reads `approved`.
- **`notify`** takes `title`, `message` and an optional `path`, and pushes to your
  devices as `notify_operator` does, its title led by the plugin's name.
- **`tools/call`** takes `name` (`<plugin>__<tool>`, another plugin's) and `arguments`,
  and calls it as the plugin: its caller context names kind `plugin` with
  `caller.plugin` set. It is decided as the operator API's calls are, by the signed
  policy and the callee's `[[plugin]]` entry, logged on the channel's external log under
  `plugin:<name>`, and never held: one that would be asked is refused, naming the allow
  that would let it run.

### SQL

The node runs no SQL of its own: the database tools are the
[`sql-review`](https://github.com/cosmicspork/sql-review) plugin's. Install it with
`tracon plugin add cosmicspork/sql-review`, and bind each database's **read-only**
account to the channels that may query it as a credential: `sql-read` is the default
profile and `sql-read-<profile>` any other, with the same `DB_*` fields `sql-review`
reads standalone (`DB_BACKEND`, `DB_HOST`, `DB_PORT`, `DB_SERVICE`, `DB_USER`,
`DB_PASSWORD`, or `DATABASE_URL`). The shipped working agreements (from bundle v25)
allow `sql-review__query` and `sql-review__describe` unattended (a node still on an
older bundle asks for them until you sign the new one); anything else the plugin offers
is asked. The read-only account is the boundary: `sql-review` classifies a
statement as a read or a write, but that is not a security control, so an account that
can write is never bound as `sql-read`.

### Jira

The node talks to no Jira of its own either: the tracker tools are the
[`jira-review`](https://github.com/cosmicspork/jira-review) plugin's. Install it with
`tracon plugin add cosmicspork/jira-review`. It is lent the channel's `jira` credential
(or `jira-<profile>`, for a call's `profile`): `JIRA_URL` with `JIRA_EMAIL` and
`JIRA_TOKEN` for Jira Cloud, or `JIRA_PAT` for Data Center. The shipped working agreements (from bundle v26) run its reads unattended
(`jira-review__search`, `jira-review__issue`, `jira-review__transitions`); its writes
(`update`, `comment`, `create`, `link`) are asked, each card showing the plugin's preview
of the change, and an approved write fails as stale if the issue moved since the card
was read. `jira-review__transition` is consequential: asked every call unless a grant
names it. A description or comment written in Jira wiki markup is shown on the card as
jira-review renders it, since it owns the `jira_wiki` format. Past approvals of the
Jira tools the node had before the plugin still open, their wiki markup shown as source.
