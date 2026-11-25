# Starting work

Work starts by typing what needs doing. A plain prompt starts an execute session
with no work item at all: the text is held as a durable draft and sent once the
harness comes up, so a closed tab or a lost connection loses nothing. Turning that
same prompt into a tracked work item — with a title, a plan phase, and a place other
sessions can pick it up from — is explicit, through **Adjust → Plan work item**
or `/api/compose`. Either way nothing about the workflow is mandatory: the channel's
phase bindings are presets, not requirements, supplying a model and budget only when
the session does not name its own; if neither does, the node falls back to a model
already in its catalogue and records which source actually won as the session's
`model_source`. **Adjust** opens repository, phase, model, and node as fields when a
session needs something other than the usual. `POST /api/sessions` and `/api/compose`
refuse a key they do not know with a 422 that names it (`error.kind` is
`unknown_field`), and suggest the field it was probably meant for: a plan session's
task goes in `work_item_id`, not `work_item`. A session that spends its budget is
held, not ended: it is paused at the end of the turn that spent it, with its harness and
context kept, until the operator raises the budget (on its page, or
`POST /api/sessions/{id}/budget`) and it carries on. A channel at its daily ceiling
starts no session at all. A session's header shows its spend live: each model call
of the open turn adds to it as it finishes, before the turn settles.

From a terminal, `tracon work start <id>` starts a session on a work item the way
the Composer does, with no request body to write:

```sh
tracon work start 71e64e12                  # plan if the item has no plan yet, else execute
tracon work start 71e64e12 --node homelab   # on a peer: its name, a unique id prefix, or `self`
tracon work start 71e64e12 --phase execute --branch feat/plugin-supervisor
```

The session starts in the item's project repository (its forge remote); an item with
no project, or a project with no remote, needs `--repo <forge url>`. The phase is
`plan` until the item's plan document exists, then `execute`. The branch is the one
the item's latest session ran on, so execute carries on from the plan's branch; with
no earlier session the node names a new `feat/tracon-<id>` branch. The model and
harness are left to the channel's phase binding and the node's `[harness] id` unless
`--model` or `--harness` names them. It prints the session id, node, phase, branch,
model and its link; `--json` prints the row. A refusal from the node (the item is
blocked or held by another session, execute without a plan, the channel at its
ceiling) is printed as the node gave it, and the command fails.

**Usage** shows what the node's model gateway counted, per channel, as seen from this
node. A ChatGPT/Codex subscription is metered in usage windows and credits rather than
tokens, so for an `openai-codex` provider the page (and the Usage today card in
Settings) also shows what the plan last reported on an answer through the gateway:
each window as "42% of 5h used · resets in 2h", the credit balance when there is one,
"limit reached (…)" after a refusal, and when it was observed. The gateway records
those `x-codex-*` headers per call beside the call's token count; it does not poll
the provider and enforces nothing from them. The ChatGPT workspace admin console
remains the authoritative credit ledger.

The model picker uses the selected runner's connected, channel-bound providers.
A sign-in starts out serving the node's default channel (or every channel the node
belongs to when it has none); choose others before connecting, or use **edit
channels** on a connected provider to change them without signing in again. A
shared sign-in reaches the members of a channel you add; a peer that already holds
it keeps its copy for a channel you remove. Each channel can use its own account:
**Sign in for other channels** on a connected subscription adds a second sign-in
beside the first, and a sign-in only ever takes over the channels it is made for, so
signing in Codex for `work` with a work account leaves `personal` on the account it
had. Each sign-in is listed with its account and channels, renews on its own, and is
disconnected on its own; the node names the channels that lose the provider before a
disconnect goes ahead.
The catalogue is declared, not discovered: **Settings → Connections → Declared
models** edits each provider's model list (id, picker name, context and output
limits, reasoning, attachments, and for the native harness parallel tool calls and
reasoning effort) and writes it to `node.toml`. The change applies without a
restart: the node asks the harness for its model list again as soon as the save
lands, so the pickers, and the next session, follow it. It asks again whenever what
it would find changes — a provider connected, disconnected or removed from
`node.toml` (its models leave the pickers), its key set, or the boundary passing a
check. When the last of those probes failed, the Providers card in Settings says why
and offers **Retry**; nowhere else does, and the pickers keep the list from before.
When a peer runs the task, choose or name the forge repository
(`github.com/owner/name`) the same way: the peer starts from its own clone of it,
cloning it with its own channel forge credential (or anonymously) if it has none.
This node's clones and browser imports are not transferred. Changing runner or channel clears the old
repository selection; changing model scope clears stale model and budget overrides.

**Repositories come from a forge.** Give the broker a `gh` or `glab` credential
(the same one publishing uses) and the form lists your GitHub/GitLab repositories.
A session names the one you pick by its identity (`github.com/owner/name`, the
`repo_path` of `POST /api/sessions`); the node clones it into its own root when the
first session on it starts, and fetches it fresh at every start. A session continues
the branch it names when the forge has it; otherwise its new branch starts from the
**Base** it names (the form offers the branches the clone last fetched, or type one), or
from the forge's default branch. A base the forge has no branch for refuses the start,
and a review of the work merges into the base. The token reaches git only
through the environment, never argv, never `.git/config`, never the stored remote
URL. A checkout on this machine is not a session source: upload its files, or run
your own harness in it and submit the result with `submit_review`. `GH_HOST`/`GITHUB_API` and `GITLAB_HOST` in the credential point the same
machinery at an enterprise forge.

```toml
# a plaintext file for `tracon credential import`, chmod 600
[credentials.gh]
channels = ["personal"]
[credentials.gh.env]
GH_TOKEN = "…"
```

**Nothing on this machine is bind-mounted into the boundary.** A session works in
a runtime-owned volume: a forge clone the node made, or files you pick in the
browser, which are copied in once (bounded in size and count, Git metadata and any
symlink leading out of the tree refused) and never written back. The harness sees `/work` and no host
path; your Git configuration, hooks, and credentials stay outside. A workspace
outlives its session — start another on it, export a checked snapshot, or download
it as a zip — and publication happens from that snapshot through a separate,
credential-bearing repository the node owns, never from the agent's clone. The
clone's `origin` is read-only and served by the gateway:
`http://<gateway>:<forward_port>/git/<channel>/<owner>/<name>.git`, named in the
harness's own gitconfig together with the session's token. The gateway speaks Git's
smart HTTP to the forge with the channel's brokered credential, which never enters
the sandbox, so `git fetch`, `git pull` and rebasing onto a `main` that moved since
launch work as anywhere else; each fetch is recorded on the session (`git_fetch`,
with the commits and refs it asked for). It forwards that one repository and nothing
else — another owner, name or channel is refused before a credential is looked up —
and it answers a push (`git-receive-pack`) itself, with a message pointing at
`submit_review`, so publication stays review-only however Git is invoked. LFS and
submodules from other repositories are not served. `fetch_branch` stays as the
brokered equivalent: it fetches on the node and carries what the workspace lacks to
it as a Git bundle. Only `refs/remotes/origin/<branch>` moves; rebasing or merging
onto it is the agent's. Neither writes anything to the forge, and neither is asked. A
snapshot is the workspace as Git sees it: what its ignore rules exclude (a
`target/`, a `node_modules/`) stays in the volume and is never exported.
Nothing is removed on its own except a check run's copy once the check is over,
unless `[gc]` sets a schedule (below).
`tracon gc` (or **Settings → System → Runtime storage**) lists the volumes
and state directories whose owner is over (an archived or vanished session, a
workspace no open session uses and older than a day, a finished check run, a
copy's `tracon-transfer-…` container a day old) and removes them with `--apply`,
the leftover containers first so what they held goes in the same run; a workspace's
directory goes only with its volume; dependency caches only with `--caches`, and the
node-wide volumes, any name it does not recognize, and another node's volumes (named with
that node's `[boundary] resource_prefix`) never.

With `[gc] every_secs` set, the node runs that same sweep with `--apply` on its own
timer (at least an hour apart; never beside a sweep asked for by hand, which is told
one is running), first archiving this node's own ended sessions (closed, out of budget,
failed) that nobody has touched for `archive_ended_after_secs`. A peer's mirrored
sessions, live and paused ones, and one whose review is still open or whose publication
is not settled are never archived. It removes only runtime storage, never session rows,
logs or evidence; `tracon gc` and Runtime storage show the last run and the next.

On Podman and Apple it lists images too, class by class with about what each frees and a total.
Every image a node builds or commits (a build-steps image, a prepared snapshot, a
repository's Dockerfile image and the `-src` image under it) carries two
labels naming its owner, `io.tracon.node.prefix` (the `resource_prefix`) and
`io.tracon.node.id`, and only images carrying this node's pair are ever removed:

| Class | Removed with `--apply` |
|---|---|
| `base` | a `tracon-sandbox-base` other than the one the node pins, once nothing is built on it. Pulled bases carry no owner label, so a node with a prefix of its own leaves them to the default node unless `--adopt-unlabelled` |
| `repository` | a repository image no current build, snapshot or open session uses. A repository no `[[repo]]` entry names, and no open session runs on, has no current build (a managed clone whose entry was removed included) |
| `snapshot` | a prepared snapshot that is no repository's current one |
| `dangling` | a `<none>` image of this node's whose tag a later build took |
| `legacy` | what older releases built and nothing runs now: the harness images and the gateway's (named from `[boundary] harness_image` and `gateway_image` where an older `node.toml` still sets them), and the layered `-claude`/`-native` session images; and, one line per tag, each `tracon-sandbox-base` tag of another release on a base image that stays (the pinned base under an earlier release's tag, or a base only other bases are built on), which goes by untagging and leaves the image. A tag stays, naming the image, while an image that is no base (a live repository image) is built on its base; only with `--legacy` |
| `unowned` | never: an image named as this node's with no owner label (made before the labels). `--adopt-unlabelled` takes it as the node's, and it is then judged as above |

Nothing a container uses (running or stopped, any node's) is removed, nothing an image
that stays is built on, and nothing made in the last hour, whose build may not have
recorded it yet. A replaced snapshot is still removed as soon as the new one is made.
An unnamed layer goes with the last image built on it, so the list names the whole chain
a removal frees, and `--apply` lists and removes again until a pass removes nothing.
On Apple, whose store records no parent image, an image stands only on the build its
snapshot label names, every tag of the build the node pins is kept, and what an image
frees is its arm64 manifests that no image that stays shares.
After the boundary converges the node logs how much is reclaimable, and removes nothing
itself.
Dependency preparation runs first as its own credential-free command in an
isolated cache: a `devcontainer.json` may name a digest-pinned image, but hooks,
mounts, sockets, and privilege in it are refused rather than partly honoured, and
`[runtime] approved_images` is the operator's list of anything else acceptable. A
repository whose `devcontainer.json` names no image prepares in the image its `[[repo]]`
entry names, so what preparation validates is what its checks run in, and reaches the
registries that entry's `egress` names and nothing else.

## What a session is told

Before its first prompt every session is handed one orientation, assembled by the
node and mounted read-only. Nothing about it is a file in the worktree, and nothing
in it can be edited from inside the session. A session started on a work item or a
review is then sent a one-line kickoff as that first prompt (plan this item into its
plan document, carry it out following its plan, or review this change), so starting
work is one action; a plain session waits for the prompt you give it.

It has two parts, and the split is the point. The **system orientation** is
tracon's own account of the installation. It opens with the work: the item and its
phase, what the phase must produce, and the plan. Then the
environment: the node and its harness, the worktree and repository, the network the
session may reach and how its dependencies were prepared. Then how to work with the
node: the tools this channel actually serves, the node's own grouped by purpose and
each offered plugin's under that plugin, by its `[ui]` title if it has one and its
name otherwise, and each `[[mcp_server]]`'s under the server's name (a harness that defers tool definitions shows the model only their
names, so this is its map), how
a call the operator decides and a wait answer, and the rules the node refuses
outright, including the fact that a refusal comes back as the rule's reason rather
than as an auth error. A review session's evidence and diff follow. It is generated
from the node's own state, so it cannot describe a tool that does not exist or an
agreement that is not enforced.

**Operator notes** are the other part: your standing text for a channel, under its
own heading, marked as yours rather than tracon's. Set them in **Settings →
Customization → Operator notes** — a name and a note; saving under an existing name
replaces it — or over the API with `PUT /api/manifest/text` (`kind =
"instruction"`). They reach every session launched on that channel from the next
launch onward; a running session keeps the manifest revision it staged.

Documents you pin to a channel follow as **Pinned documents**, in full. Your directives, and confident
facts about the session's own project, are under **Known**; a fact retained for the
whole channel is left to `recall`. The other ready work on the project is
discretionary: it is included only as far as the session's context budget allows.
Everything left out is named at the end of the orientation with the call that
fetches it, so a session can ask for what it did not get.

## Before a compaction

A compaction replaces the conversation the model works from with a summary, and a
harness the node starts again after its process died rebuilds its context from its
record. Before either, the node saves what the summary is of, so it still exists:

- **the full-text conversation**: the session's log rendered as Markdown
  (`conversation.md`: the orientation, every prompt, the agent's messages and
  thinking, each tool call with its input and result, earlier summaries), and the
  harness's own record of it (`transcript/`: Claude Code's `projects/<dir>/<id>.jsonl`
  and its subagents', `tracon-agent`'s journal), where tool output the log cut short
  is whole;
- **the scratchpad**: every session is given a scratchpad directory beside its harness
  state (`~/scratchpad` in the sandbox; the orientation names it) for notes, plans and
  findings it wants to outlast its context. It is not part of the worktree or the
  change.

A compaction you ask for (the session's **Compact**, `POST /api/sessions/{id}/compact`)
is saved before its turn is dispatched; one the harness starts itself is saved as it
says it has begun (Claude Code's `compacting` status, `tracon-agent`'s
`context_compacting`), or as it ends when it said nothing first. Both harnesses append
a compaction to their record rather than rewriting it, and the node takes the running
sandbox's trees back first, so a save holds everything before it. Each save is a
`context_saved` line in the log.

The saves are kept in the session's scratch volume under `saved/<index>-<before>/`, the
package its harness state is backed up in, which no sandbox mounts: they are there
again when the session is reopened or its harness restarted, and they go only when its
runtime storage is reclaimed (**Settings → System**, `tracon gc`), with its harness state. Read one back from
the session: open its `context_saved` line in the log to read the conversation, or
**Export** it as one JSON document holding every file
(`GET /api/sessions/{id}/saved`, `…/saved/{index}`, `…/saved/{index}/export`;
`tracon session saved <id> [<index>] [--export <file>]`). An export is plain JSON,
readable with nothing running; like any transcript it can carry what the session saw.

## Review before publish

An agent has no forge token and never runs `gh` or `glab`. To publish it commits,
submits, and waits: the node snapshots the workspace volume itself, runs the
project's checks against that snapshot in a throwaway container, and refuses a failure
or an oversized diff before you ever see it. Which checks, and what they run in, is that
repository's `[[repo]]` entry in `node.toml`: its own commands and the image its recipe
builds, or the node-wide `[supervision] checks` and the stock base where you
named none — and a check whose command is not in the image is reported as *not runnable*,
naming the missing tool and the image it looked in, rather than as a failing check the
agent is told to fix.

Every runtime runs a repository on the stock base, so its toolchain is installed by
build steps (next section), or by a Dockerfile that begins `FROM ${TRACON_SANDBOX_BASE}`
(`dockerfile = ".devcontainer/Dockerfile"`). An `image`, or a Dockerfile whose first
`FROM` is anything else, is refused. The node builds from the repository's **default
branch**, read out of Git, never from a working tree or a candidate, which could
otherwise choose the image its own checks run in. It pins what it built, and rebuilds
when the recipe changes there. Only the Dockerfile is honoured: a devcontainer's hooks,
features, compose files and services are not, and `prepare` is where their work
belongs. The first check (or `tracon repo build`) pays for the build; a build that
fails leaves the last image that worked in place and is not retried until you ask.
`tracon repo ls` shows each repository's image and any command it has no tool for.
`tracon repo build host/owner/name` on a repository no session has cloned yet clones it
first, with the node's forge credential or anonymously, so `repo set` then `repo build`
works on a fresh node.

### Repository recipes on the stock base

Sandboxes run on the stock base on every runtime (on Kubernetes, a recipe needs
`[runtime.kubernetes.builder]`), so a repository's recipe lives in `node.toml` and
nothing goes in the repository. A devcontainer stays the developers' environment and is no longer the node's recipe. An entry looks like this:

```toml
[[repo]]
path = "github.com/owner/app"
build = [
  "apt-get update",
  "apt-get install -y --no-install-recommends php8.5-gmp librsvg2-bin",
  "rm -rf /var/lib/apt/lists/*",
]
prepare = ["composer install --no-interaction --no-scripts", "npm ci --ignore-scripts"]
egress = ["apt", "packagist", "npm"]
checks = ["just check"]
```

- **`build`** runs as root at image build time, each step one `RUN` of `sh -ec`, on the
  base pinned by digest when the node starts: the digest of the host architecture's own
  manifest, not the release's multi-platform index, so a release that ships the same base
  image keeps every image and snapshot keyed on it. The node writes the Containerfile itself
  (`FROM` the base, the steps as root with `HOME=/root`, so nothing they cache lands in the
  harness's home, then back to user 65532, `HOME=/home/harness`, `/work`)
  and builds one image per repository: no build context and no layer over it. The image
  is keyed on the base's digest, the steps and the egress, so a new base, an edited step
  or a host you add rebuilds it; a build that failed is not retried until one of those
  changes or you ask (`tracon repo build`).
- **`prepare`** then runs as uid 65532 on the default branch, in a sandbox from that
  image, which is snapshotted for the repository's sessions and checks as for any
  repository on the base. The snapshot keeps what `prepare` left in `/work`
  (`node_modules`, `vendor/`) and `/cache`, without the branch's tracked files, and
  records the commit it was prepared from.
- **Egress.** The steps reach only the entry's `egress`, through a grant of their own on
  the node's egress proxy. On Podman the build has no network but one loopback port to
  that proxy (`pasta --splice-only`), and no DNS: a step gets `HTTPS_PROXY` and nothing
  else. The proxy carries HTTPS only; the base's apt sources already are, and the `apt`
  preset names them. On Apple the steps don't go through `container build`, whose
  builder VM has a network of its own: they run as root in a sandbox from the base,
  with no network and one loopback tunnel to that grant, which is then committed and
  given the base's user, home and directory. On Kubernetes the steps build on the
  cluster's BuildKit (`[runtime.kubernetes.builder]`), whose rootless steps share the
  daemon's own network, which nothing in the node can narrow: each build gets the grant
  at the node pod's address on the egress port, as BuildKit's predefined proxy
  arguments (`HTTPS_PROXY`, `https_proxy` and the plain-HTTP pair), which every `RUN`
  sees and no image history or cache key keeps. A tool that honours a proxy is held to
  the list; a step that goes around the proxy is held only by the daemon's
  NetworkPolicy (see [Images on Kubernetes](install.md#images-on-kubernetes)). BuildKit reuses a cached `RUN` (same
  base digest, same command) whichever build ran it, so on a daemon shared with
  other clients a step held to the list here may be a cache hit of one that was
  not: give the node a daemon of its own, or set `[runtime.kubernetes.builder]
  no_cache = true`.

Settings → Repositories offers it as *Build steps on the stock base*, and
`tracon repo set <path> --build "apt-get update" --build "…" --egress apt` sets it from a
shell. On any other node the key is refused at load, naming the setting it needs.

Install what sessions need where uid 65532 can read and run it: under `/usr/local`
(its `bin` is on `PATH`), not root's home. For rustup, set `CARGO_HOME=/usr/local/cargo`
and `RUSTUP_HOME=/usr/local/rustup` in the step and link `cargo`, `rustc` and the rest
into `/usr/local/bin`. A Rust repository, for example:

```toml
[[repo]]
path = "github.com/owner/rust-app"
build = [
  "apt-get update && apt-get install -y --no-install-recommends libssl-dev && rm -rf /var/lib/apt/lists/*",
  "curl -fsSL -o /tmp/rustup-init \"https://static.rust-lang.org/rustup/dist/$(uname -m)-unknown-linux-gnu/rustup-init\" && chmod +x /tmp/rustup-init && CARGO_HOME=/usr/local/cargo RUSTUP_HOME=/usr/local/rustup /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain 1.98.1 -c clippy -c rustfmt && rm /tmp/rustup-init && ln -s /usr/local/rustup/toolchains/1.98.1-*/bin/* /usr/local/bin/",
]
prepare = ["cargo fetch --locked"]
egress = ["apt", "crates", "static.rust-lang.org"]
checks = ["cargo test --locked"]
```

The links are to the toolchain's own binaries, not rustup's proxies, which would need
`RUSTUP_HOME` set wherever they run. So nothing reads the repository's
`rust-toolchain.toml`: name the same version in the step.

**From a devcontainer Dockerfile.** Take its `RUN` lines as steps, in order, and drop
what the base already has (git, curl, a C toolchain, jq, Node, Python with uv, PHP 8.5
with Composer: see `containers/sandbox-base/Containerfile`). `ENV` and `ARG` values go
into the steps that read them (`export NAME=value && …`); a `COPY` from the repository
becomes a step that writes or fetches the file, since there is no build context. Name
the hosts the steps fetch from in `egress`, then drop `dockerfile` (or `image`): an
entry names one of the three, and `tracon repo set <path> --build …` drops the others
for you.

The table is edited where you would look for it: **Settings → Repositories**, or
`tracon repo set <path> --check "just check" --prepare "cargo fetch --locked"
--egress crates` (and `show`, `rm`). Either writes
`node.toml` and takes effect for the next session and the next check without a restart —
a restart would end every session on the node to change one repository's environment. A
session already running keeps the image and the egress it started with. A hand edit of
`node.toml` is picked up within about 15 seconds, the same way.

Or let a session draft it. `repo_setup_draft` reads the default branch (its devcontainer,
lockfiles, a `just check` recipe or `package.json` scripts) and drafts the entry with where
each field came from. `repo_setup_try` runs a draft the way required checks run, in a fresh
container. A draft that names a Dockerfile is tried in the image built from it, never on the
stock base instead. An image this node already built from the same Dockerfile and context
is reused; building a new one is yours to allow on a card, because the build runs with the
network outside the boundary, and the entry you later allow uses that image rather than
building it again. A trial prepares under an egress grant of its own, so a host its
preparation is refused is listed in the trial's `refused` while it runs, and named in its
failure even when it ran out of time. A trial being refused will not recover, and the
session can cancel it rather than wait it out. `repo_setup_propose` puts it to you on a card, which you may edit. The node
writes the entry only when you allow it, and a proposal never opens the repository's
egress to its sessions. Hosts you opened to the session while it tried are suggested in
`egress`. A draft never has `build` steps. Turn a drafted `dockerfile` into steps
yourself, as above: unless it begins `FROM ${TRACON_SANDBOX_BASE}`, it is refused.

A session runs in that image too, with Claude Code or `tracon-agent`. Sandboxes run on
the stock base on every runtime, so the harness comes from the bundle mounted at
`/opt/tracon`, and the session runs in the repository's build-steps image, or its
prepared snapshot, with no layer of the node's. The session's start line says which
image it got, and why when it is not the repository's. A repository with no image yet
builds one at its first session, which waits; one whose recipe changed starts on the
image it has and is rebuilt behind the session.

**Offer a repository's recipe to another node.** On a mesh, an entry's **Offer to…** in
Settings → Repositories sends its recipe to the other members of a channel you pick:
the Dockerfile and its context or its build steps, the checks, the preparation, its
registries and whether sessions reach them, and the time limit, with the repository named
by its forge identity (`github.com/owner/name`). Never its pinned image or any digest, the
credentials it lends to preparation, its path, or how its commits are published; a command
naming a home directory is refused. Build steps run as root on the receiving node's own
stock base when its image is built, so the accept card shows them in a section of their
own, against the entry's current steps, with **Apply these build steps** ticked: accepting
writes them in place of the entry's own image source (a pinned image only if you choose to
replace it), and unticking keeps the entry's own `build` or Dockerfile exactly as it is. An
entry with no build steps offers none, so a node from before they travelled still takes it. Each receiving node only stages the offer and notifies its
operator, who sees what would change on the entry it lands on (the entry already
answering for the repository by name, or one they name) and accepts or declines it.
Offering and accepting are admin actions, confirmed with a passkey from off the node, and
an accept writes exactly what was reviewed or nothing. Accepting keeps the entry's
comments and its own fields, except that credentials the entry lends its preparation are
dropped when the offer changes what that preparation runs or reaches, unless you choose
to keep them: the repository is then held, and nothing builds or warms its cache with
them until you start its build from Settings, which asks for your passkey off the node
and says what it lends. A later offer that leaves that preparation as it is keeps the
repository held; rewriting its `prepare` or its credentials yourself ends the hold. The
node then builds and pins
its own image. An undecided offer is replaced by a newer one from the same node for the
same repository, is withdrawn if that node leaves the channel, and lapses after 14 days.

**Take a co-member's build instead of building.** The recipe is shared and the image is
not: each node pins its own. A Podman node with DigitalOcean's container registry
(`[digitalocean] registry` under `registry.digitalocean.com` and a `credential`) also
pushes each repository image it builds there, as
`<registry>/<prefix>-repo-images:<recipe>-<arch>`, and sends the other members of its
channels the digest, signed with its node key, with the repository's forge identity, the
recipe's hash and the platform. A Podman node about to build the same recipe for the
same platform (a recipe it accepted, on the same pinned base) pulls that digest instead,
with its own DigitalOcean token, and pins it as its own build: nothing builds, and the
build's log names whose image it took. It takes one only when the statement verifies
under its signer's key, the signer still shares the channel it came on, and the registry
serves exactly the digest signed, the pull included. A node with no DigitalOcean token or
no read access to the registry, or one whose image fails any of that, builds the recipe
itself, and that build's log opens with why. `tracon repo build` always builds here. A
node that builds its own pins its own digest, and pushes and signs it in turn; only
what a node built is pushed, never what it took. Kubernetes nodes build into their
builder's registry and neither push nor take these.

A session reaches no registry unless you say so. `session_egress = true` on the
repository's entry opens its `egress` list to that repository's sessions, through proxy
credentials that are the session's own, so "add this dependency and run the tests" is an
ordinary task: `cargo add`, `bun add`, `composer require` work, and a host the entry does
not name is refused. It is opt-in because a session is long-lived and runs what a model
decides: a host that accepts uploads accepts them from it, and the `github` and
`packagist` presets both carry `api.github.com`. This holds for Claude Code and
`tracon-agent` sessions alike: the grant replaces the harness proxy for the harness and everything its
shell runs, while the model and the node's tools stay on the gateway, which no proxy
sits in front of.

A private registry (Laravel Nova, a private npm scope, Private Packagist, GitHub
Packages) is reached with a credential the node lends to one preparation only: the one
that warms the repository's base cache from its **default branch** after the node builds
its image. Every candidate, session and check then installs from that cache and never
holds the credential, because each of them runs code an agent can change. Put the
credential in the broker as environment, naming the repositories that may use it, and
name it from the entry:

```toml
# a plaintext file for `tracon credential import`, chmod 600
[credentials.nova]
repos = ["github.com/acme/shop"]    # written as a [[repo]] path is, or a group as "gitlab.com/acme/**";
                                    # empty lends it nowhere
[credentials.nova.env]
COMPOSER_AUTH = '{"http-basic":{"nova.laravel.com":{"username":"…","password":"…"}}}'

# node.toml
[[repo]]
path = "github.com/acme/shop"
dockerfile = ".devcontainer/Dockerfile"
prepare = ["composer install --no-interaction --no-scripts"]
egress = ["packagist", "nova.laravel.com"]
prepare_credentials = ["nova"]
```

`tracon repo set github.com/acme/shop --prepare-credential nova` sets the same list
(repeat the flag for several; `--prepare-credential ""` clears it). Lending a credential
this way or in Settings, or lending a different set, warms the base cache again with it straight away,
without a rebuild; clearing the list warms nothing. Both sides must agree: an entry naming a credential that does not name the repository
is refused at the warm-up, and the warm-up does not run. A credential's `repos` can name a
forge group instead of each repository: `repos = ["gitlab.com/acme/**"]` names every
repository under `gitlab.com/acme`, by whole segment (`gitlab.com/acme2/x` is not one), and
each of them still has to name the credential in `prepare_credentials`. `**` is allowed
only as the last segment, and nothing else is a wildcard; an import that globs anywhere
else is refused. Settings lists the configured repositories each credential covers. A
prefix widens what can read the value: every repository under it, on whichever entry opts
in, has its default-branch install, and that install's Composer plugins and npm lifecycle
scripts, run with the credential, so a repository added to the group later gets the same
reach without anyone editing the credential. Read the secret from the
environment (`COMPOSER_AUTH`, or an `.npmrc` line like
`//npm.pkg.github.com/:_authToken=${NPM_TOKEN}`) and never write it to a file: the cache
the warm-up fills is copied into every session on the repository. What preparation
prints is shown with the lent values taken out. The values never appear on a container's
command line or in a Kubernetes pod spec: they travel with the spawn over the sandbox
protocol (see [As a Kubernetes pod](install.md#as-a-kubernetes-pod)). Two limits follow from lending it so
narrowly. A candidate that adds or upgrades a private package cannot fetch it, so that
change is checked after it merges and `tracon repo build <repo>` warms the cache again.
And the warm-up runs whatever the default branch's install runs, so a Composer plugin or
an npm lifecycle script there could read the credential and send it to any host in
`egress`; install with scripts off where the project allows it.

**Attaching a credential instead of lending it.** With `[upstreams] preparation =
"attach"` the warm-up is lent nothing: no `COMPOSER_AUTH`, no token, nothing in its
container that a plugin or lifecycle script could read. The operator declares the host
each credential is for, and the node writes the credential into the warm-up's requests to
that host itself:

```toml
[upstreams]
preparation = "attach"              # "env" (the default) lends environment as above

[[upstreams.host]]
host = "nova.laravel.com"           # exactly this host; reached on 443 over the node's own TLS
credential = "nova"                 # an `env` credential, bound by its `repos` as above
attach = "composer"                 # its COMPOSER_AUTH `http-basic` (or `bearer`) entry for the host
# attach = "basic"                  # or: username = "${USER}", password = "${PASSWORD}"
# attach = "header"                 # or: header = "Authorization", value = "Bearer ${TOKEN}"
# sessions = true                   # attach it for sessions too, in channels the credential is bound to
```

The client asks its own egress proxy for `http://nova.laravel.com/…` in plain HTTP. The
node, holding that client's grant, opens TLS to `https://nova.laravel.com` (verified
against the web's roots), drops whatever `Authorization` the client sent, writes the
credential, and hands the answer back. Nothing is intercepted: a CONNECT tunnel to any
host, the declared one included, stays end to end and carries nothing, and is still
`egress`'s to allow. A credential is attached only to its declared host, never to a
lookalike or another port, and only for a client whose grant carries it. The node follows
no redirect; one back to the declared host is pointed at the proxy again, and one anywhere
else reaches the client as sent and carries nothing when followed. Every credential the
entry names needs a declared upstream, or the warm-up does not run.

Composer asks `https://` URLs, and keys its download cache by URL, so the install has to
ask the plain one everywhere the cache is read: in the warm-up, in a candidate's
preparation and on a session's workspace. Rewrite a copy of the lock rather than the
tree's own, so the workspace never shows a change:

```toml
prepare = ["u=$(mktemp -d) && cp composer.json composer.lock \"$u\"/ && sed -i 's#https://nova.laravel.com/#http://nova.laravel.com/#g' \"$u/composer.lock\" && COMPOSER=\"$u/composer.json\" composer config secure-http false && COMPOSER=\"$u/composer.json\" composer install --no-interaction --no-scripts --prefer-dist"]
```

Composer installs into the workspace's own `vendor/` either way. Only the warm-up is
credentialed; elsewhere the plain URL is refused at the proxy, so a package not already
in the cache fails as it would have without the credential. With `sessions = true` a
session can ask the declared host as the credential's owner, though it never holds the
credential; leave it off unless an agent needs that.

A refused host is put to you rather than left to fail quietly. The refusal raises a card
naming the session and the host, once however often a package manager retries, and its
reason line — the line `pip` or `npm` prints — tells the agent the operator has been
asked and to retry once you answer: a connection cannot be held open while you decide.
**Allow once** opens the host for the next ten minutes, **For this session** until the
session ends, and **Save to the repository's egress** also writes it into that
repository's `[[repo]]` entry in `node.toml` (making one if there is none), so later
sessions start with it; an entry whose egress is open to its preparation only is not
flipped open for sessions by an answer about one host. Whatever you choose opens that
session's own grant and no other. An agent that knows what it needs asks first with
**`request_egress`**, which waits up to 45 seconds for your answer; once you decline, a
retried connection is told so and does not ask again. The session's log shows each
refusal, and the orientation it starts with says all of this before its first command.
It works wherever grants are issued: on every runtime, through each sandbox's tunnel to
the node's egress proxy.

Web pages are not egress. A session's `curl`, `wget` and the harness's own fetch tool
(Claude Code's WebFetch) are refused at the boundary like any other host, and the
refusal's body and the orientation tell the agent to call **`web_fetch`** instead: the
node fetches the page and hands back its status and body. It is deny by default. A
URL the channel's fetch allow list covers is fetched at once; any other raises a card
with three answers: **Fetch once** fetches that URL now and nothing more, **Save to
channel** adds the card's pattern (the URL's origin, `https://docs.rs/*`, unless the
agent proposed a narrower or wider one, and you may edit it before saving) to the
channel's allow list and then fetches, and **Deny** tells the agent to do without it.
Saving is an admin answer, confirmed with a passkey away from the node. A fetch reads
at most 10 MB of body and follows at most five redirects, each one decided against the
same list before it is followed: a redirect to a URL the list does not cover stops and
names the URL, for the agent to ask about on its own. The node never dials an address
its egress proxy refuses, and every fetch, failed ones included, is on the session's
log as evidence: the URL, the final URL, the status and the body's SHA-256. Claude
Code's WebSearch is different: the provider runs the search, so the request only
reaches the provider through the model gateway, which forwards it as asked.

A repository's warm-up asks too. A registry often sends its downloads to another
host (Nova's go to a Cloudflare R2 bucket), and the warm-up is the first thing to find
out. Its refusal raises a card naming the repository and the host that offers only
**Save to the repository's egress** or **Reject**, because the warm-up is over before
anyone answers. Saving adds the host to the entry's `egress` without touching
`session_egress`, so sessions reach it only if the entry already opens its egress to
them, and then warms the cache again. The card goes on the channel of the repository's
newest session, or `[session] default_channel`. With neither, nobody is asked and the
warm-up's failure names the host. A candidate's or a session's preparation runs code an
agent can change, so its refusal asks nobody. Every preparation's failure names the
hosts it was refused, in the node's log, a check's "not run" line and a session's
orientation, since a package manager reports a refused host only as a failed download.

A check has no network. A repository whose checks need its dependencies names `prepare`
commands (`bun install --frozen-lockfile`, `composer install`, `cargo fetch --locked`)
and the registries they may reach (`egress = ["npm", "crates"]`). Those run once per
run, before the first check that actually executes, on a copy of the candidate, with a
dependency cache writable and proxy credentials of their own that reach exactly those
hosts; the credentials are revoked before any check starts. Each check then gets its own
copy of the prepared tree and the cache read-only, so one check's output is never
another's input and none of them prepares again. Three things follow, and they are yours
to weigh. An install runs the candidate's own lifecycle scripts unless the command says
otherwise (`--ignore-scripts`, `--no-scripts`); what it can write is that run's cache and
no other. The cache a run prepares into is its own copy of the repository's *base* cache,
which only a preparation of the default branch ever writes — the node makes one when it
builds the repository's image — and it is removed with the run, so nothing one candidate
installed is ever in another's evidence; `tracon gc --caches` empties the base. The grant
filters by host, not by method, so a registry that accepts uploads accepts them from that
preparation — and from nothing else: a session and another preparation each hold their
own grant, are refused each other's hosts, and never wait for one
another. And the image's default user has to be able to write `/work` and `/cache`, which
under rootless Podman means it runs as root.

A session in its repository's image gets the same preparation before its harness starts:
the `prepare` commands run on its workspace, with its own session cache writable and the
same egress grant, so `node_modules` and the like are there when the agent arrives. It is
bounded by the entry's `timeout_secs` and stopped with the session; a failure is recorded
in the session's log and told to the agent in its orientation, and the session starts
anyway.

A session that starts from its repository's prepared snapshot (on Kubernetes, once
`[runtime.kubernetes.builder]` can make one) skips `prepare` when its workspace is at the very commit the snapshot was prepared from:
its tree is sent over the snapshot's `/work`, which already holds what `prepare`
installed, and its log says *workspace prepared from the snapshot*. At any other commit, or for a resumed workspace, `prepare` runs again,
every step in one sandbox from the snapshot on its warm `/work` and `/cache`, and the
workspace comes back once at the end. The snapshot is keyed on lockfiles, not commits, so
it stays current as the default branch moves, and sessions on a later commit prepare
again on top of it. A `Cargo.lock` counts only for what it fetches: its registry and Git
packages. A release that bumps the workspace's own versions, or a path dependency added
between its crates, keeps the snapshot. A session's sandbox from a snapshot is never sent the node's copy of
its cache: the snapshot's `/cache` is the warm one. A candidate's preparation before its
checks always runs, since it prepares a tree under review rather than a commit the
snapshot was made from.
The review's diff reads each file under its own header, side by side or unified:
side by side, the two sides aligned row for row, when the screen is wide enough,
unified when it is not, and the layout you pick is remembered on that device.
Each side carries the file's own line numbers (unified, a column for before and one for
after), the code is highlighted by its language, the words that changed inside a line are
marked apart from the rest of it, and long lines wrap when you turn **Wrap lines** on.
Every hunk header says how many unchanged lines the diff leaves out above it, and
**Show** fills them in. Those lines are read from what the revision pinned (the base
blob its diff names and the file at its commit, or the evidence store's copy of that
commit), never from the worktree as it stands, and they are checked against the diff
first; when they cannot be read or do not match, the gap says so instead. A rename, a
binary file and a change of mode are each labelled as what they are.
To find your way through a large change, the bar above the diff filters the files by
path, totals the lines added and removed, folds or opens every file, and steps to the
next or previous change (**n** and **p** on the keyboard). Each file folds under its
header with its own counts and a **Viewed** mark. A mark is your reading progress on this
device, bound to the revision you made it on and to the file's diff as it was then: when
the agent resubmits, files it left alone stay viewed and files it changed lose the mark
and say they changed since you viewed them. Viewed never means approved; nothing is sent
when you mark a file, and only a verdict decides the review.
A large review stays responsive: every file's header shows at once, and a file's diff is
drawn only while it is near the view (and highlighted only then), one file at a time.
Generated files (lockfiles, minified bundles, vendored or generated code, snapshots, or a
file that says it is generated) and oversized ones (over 1,500 changed lines, or a line
over 5,000 characters) start folded with the reason and a **Load diff** button.
`bun scripts/diff-bench.ts` in `spa/` measures a review of a few hundred files in Chromium.
You approve, reject with a reason, or — on a desktop — edit the diff and send it back as
a request for changes. A request for changes is how a review comes back: the agent
resubmits with the same `review_id`. A rejection is final: `submit_review` refuses a
rejected review's id, quoting your reason, and the agent submits the work again as a new
review if it continues. Approval publishes exactly the reviewed tree with the brokered
credential; if the branch moved since submit, approval is refused and the changed files
are named. The tree is what was reviewed; the commit message and branch it ships with are
prose you edit beside the diff, where the agent's own commits are listed. By default the
node pushes one commit holding the reviewed tree with the approved message on the
approved branch. An update adds that commit to what the change holds, and when the
revision merged its base (to resolve a conflict, say) the commit keeps that base as a
second parent, so the forge sees it merged. `commits = "keep"` (in `[publish]`, a repository's entry, or the
channel's `publish.commits` binding) pushes the agent's commits instead: as written, or
with the messages you edited on the review, each remade with the same tree, author and dates.
Subject and branch rules (`style`) are off unless configured, and a submission that
breaks them is refused before it reaches you.

A review either opens a new pull or merge request or updates one the branch
already has (`change`, named at first submit). The review's title and body are the
agent's summary for you; what reaches the forge is separate and editable before
you approve: a description (a new change's, or a replacement for an existing
one's), a comment, both, or — for an update — nothing but the push. They are
edited as Markdown source, and below them the review page shows what the forge
gets, rendered (sanitized, nothing fetched) or as the exact source: the node
composes it with the same function approval publishes with, so it includes
anything the node adds, such as the opener and fold of `[review] generated_details`. Submit checks
the forge first: a branch with an open change must name it, and an update must be
an open change of that repository into that base. An update is pushed to the
change's own branch, whatever the worktree's is called. A review that opened a
change goes on updating it: resubmitted after it was published, its next revision
comes back to you on the same card. An update pushes only over the
commit the change held at submit; a rewritten history (rebase, amend) must be
declared, is forced with that commit as its lease, and always comes to you even
under a publish grant. Until a change is opened, a resubmission may name another
`base` (a stacked base that was merged and deleted, say), and a declared rewrite
leases on whatever the branch holds on the forge, so a publication that pushed
and then failed to open can be rebased and retried. Once a change is open, its
base is moved on the forge, and a resubmission naming another is refused.

A review can also push straight to its base, with no change: submitted with
`branch` equal to `base`, approval fast-forwards the base to the approved
revision (squashed onto it with the approved message, or the agent's commits under
`keep`) and opens nothing, so the forge's CI runs after the push rather than before
it. Any review may ask for it. Publication looks first and refuses if the base moved
since the revision was captured; the push is never forced, a rewrite is refused, and
a resubmission under the same review pushes its next revision the same way. While
`warn_direct` is on (the default; `[publish]`, a repository's entry, or the channel's
`publish.warn_direct` binding), the review, its card and its notification say so in a
warning that names the commit it fast-forwards from. The node also asks the forge
what protects the base, at submit and when the review opens (GitHub's rulesets and,
where the credential can read it, classic protection; GitLab's protected branch), and
shows what it found: a base that requires a pull request is named as a push that will
be rejected, and a lookup that fails reads *protection unknown* with its reason. The
forge is the authority: submit is not refused on it, and a push the forge rejects is
a publication error carrying the forge's message, never retried or worked around.

Feedback left on the forge comes back the same way: an agent reads a change's
review threads (file, line, resolved or outdated) and conversation with
`pr_threads` / `mr_discussions`, finds a branch's open change with `pr_for_branch` /
`mr_for_branch`, and sees each reviewer's latest verdict in `pr_status`, all
unattended. Replying to a thread, and resolving it, is `pr_reply` / `mr_reply`,
which you are asked about like any other comment. So is a comment on a GitHub issue,
`github_issue_comment`. So is marking a draft
ready for review, or turning it back into a draft with `ready: false` (`pr_ready` /
`mr_ready`): one already in that state is left alone, and a closed or merged one is refused. CI reads the same on either
forge: a run's jobs and the step that failed (`run_status` with `run_id` /
`pipeline_status`), the runs at an exact commit (`run_status` with `sha` /
`pipeline_list_by_sha`), and the end of a job's log (`run_logs` / `job_trace`, 16 KiB
unless asked, at most 64) are unattended, and so is waiting for one to move (`run_wait` /
`pipeline_wait`). Every CI write is asked, and the node follows the run or pipeline it
started: retrying the failed jobs (`run_rerun` / `pipeline_retry`), starting a run on a
branch (`run_dispatch` / `pipeline_run`), and on GitLab playing a manual job (`job_play`)
or, under a scoped grant, a deployment job at an exact commit (`deploy`). Starting a run
never takes a tag or a value naming production. `pipeline_run` also takes the
pipeline's `spec:inputs` as `inputs` (name to string, boolean or number, sent with
their types, e.g. `{"build-dev-images": true}`), which variables cannot set; the
card shows each input's value, and one whose name or value names production is
refused before you are asked. `run_dispatch` starts only a workflow you list for
the repository:

```toml
[github.dispatch]                   # what run_dispatch may start; anything else is refused before you are asked
"owner/name" = ["renovate.yaml"]    # file names in .github/workflows
```

A channel holding both credentials is offered both forges' tools; a session's own
repository only fills in the `repo` or `project` argument for its forge's tools, it
hides neither set. `tracon provenance <sha>`
answers, later, which model, which prompts, which approval and which policy shipped
a commit.

What you review is a *candidate*: an immutable capture of the commit's tree,
kept even after the review moves on. Required checks come only from the node's
configuration (a repository's own `.tracon/checks` cannot replace them) and run
against a writable copy of the candidate in the runtime; each run keeps its
command, inputs, the image the runtime actually executed, the log, and the outcome.
A resubmission that changes only the title, description, or ticket prose reuses
the code evidence of the same candidate and says so; changed code, checks, or
image runs again. Every decision — yours or a policy grant's — is recorded
against the revision it decided, with the requirements pinned as they were when
it was submitted. A curated demonstration (a document with commands, output,
and images) can be attached beside the authoritative record; it is linked and
hashed, never executed, and flagged stale if the document changed since.

A diff alone often does not say whether the work is done: a changed screen, a
before and after, a short account of what was tried. An agent shows you that
with **`show_work`**: a Markdown account, an HTML page with the images it loads,
or plain files from its workspace, read from the node's own snapshot of it. The
review shows it beside the diff, and the session keeps it; a page renders only on
the isolated preview origin, sandboxed, with no network, and the account's images
are named rather than fetched. It is the agent's account, not verification: the
node vouches only for the commit it was shown at, marks it stale once the
candidate moves past that commit, and lists the required checks it ran there.
None of it goes to the forge; the pull request gets prose only. A harness you run
yourself shows work on a review it submitted, from that review's worktree.

An agent can also show a **proposal**: `show_work` with `proposal: true`, usually
an HTML page bundled with what it loads, setting out layouts or flows for you to
choose from. It is attached and rendered like any other shown work, but under
**Proposals** rather than **Shown work**, tagged *proposal* on the session and the
review, in the session log, and in a reviewer's orientation, so it is never read as
an account of what was built; it is not counted among the session's claims. A
**plan session** is offered `show_work` for this alone: everything it shows is a
proposal, and it is told to say in its plan which option it assumes and to ask you
when the choice is yours.

Some of what a session builds is slow to rebuild and the same every time: "before"
screenshots of the default branch are the first case. A session keeps such bytes for
later sessions on the same repository with **`stash_put`** (`key`, `inputs`, `path`)
and asks for them back with **`stash_get`** (`key`, `inputs`). An entry is found by
its name and a digest of the inputs it names: files and directories of the workspace,
or Git objects written `<revision>:<path>` (`origin/main:spa`). The node computes
that digest itself from its own snapshot of the workspace, so a changed input misses
(`hit: false`, with how many entries under that name were built from other inputs)
and is never handed back stale. A hit is unpacked over the session's workspace in its
running sandbox, at the path it was kept from. Like `show_work` it reads only what
Git would see, so what is stashed is written to a path no ignore rule matches and kept
out of commits. The stash is sessions only: execute and review sessions are offered
it, a plan session and a harness you run yourself are not, and it is never mounted
into a run, so checks and preparations never read it and nothing an agent stashed
reaches a check. It holds bytes, not knowledge: what an agent learned belongs in a
memory. Each repository of each channel keeps up to 1 GB on the node that ran the
session (under `stash/` in the state directory); a put past that evicts the least
recently used entries, a hit counting as a use. **Settings → System** shows what it
takes as *Artefact stash*. Neither tool is asked: they reach nothing beyond this
node.

**Work → Evidence** browses captured candidates without requiring an ID. Reviews,
sessions, and work items link to the corresponding owner and channel; a peer's
candidates are read from that owner. A missing local candidate is not evidence
that a peer has none.

### Review threads

Feedback on a review can be a set of threads instead of one note. Under the diff,
open a thread on the whole change, on a file, or on a line or range: pick a line
number in the full diff's gutter, side by side or unified, to start one there, and
another to make it a range. Lines that carry a thread read bold in the gutter. The
since-last-verdict diff offers no line numbers to pick. Each
thread is bound to the revision on screen and, unless general, to a path and a side
(the change, or the base). The node reads the lines from that revision's diff
itself, and refuses a line the diff does not show.

**Request changes** sends the threads still open to the agent with your note, which
may be left out when a thread says it all, and with an edited diff if you made one.
`review_status` returns them under `threads`, each with its revision, path, side,
lines and comments. When the agent resubmits, it names in submit_review's `threads`
each one the new revision answers, marked `addressed` with a reply, or replies
alone where it left one as it was.

Threads are kept across resubmission. Each reads one of four states: *unresolved*,
*addressed* (the agent says so), *outdated*, or *resolved*. A line thread whose lines
read differently on the new revision, or a file thread whose file's diff changed,
is **outdated** there: it is never moved onto other code, and still shows the lines
it was written on. Reply to a thread, resolve it, or reopen one that is addressed
or resolved. A peer's review's threads are held by the node that owns the review:
they are read from it and changed through it, and wait while it is unreachable.

## Databases a session can start

A session starts what the node's `[[service]]` catalogue offers with
`service_start`, by name, in its own sandbox: reached on its 127.0.0.1, gone
when the session ends. Postgres and Redis are offered once their bundles are in
the catalogue. Build them with `scripts/database-bundle.sh` and point an entry
at one `<version>/<arch>` of each:

```toml
[[service]]
name = "postgres"                     # superuser postgres, trust, on 127.0.0.1:5432
bundle = "/srv/tracon/postgres/17.10/amd64"
port = 5432

[[service]]
name = "redis"
bundle = "/srv/tracon/redis/7.4.8/amd64"
port = 6379
```

Each bundle carries its clients (`/opt/tracon-services/postgres/bin/psql`,
`/opt/tracon-services/redis/bin/redis-cli`). By default a database starts empty
in every session. A repository whose sessions should find what the last one left
names the services in its entry:

```toml
[[repo]]
path = "github.com/owner/app"
service_volumes = ["postgres", "redis"]
```

(or **Settings → Repositories → Service data kept**, or `tracon repo set
github.com/owner/app --service-volume postgres --service-volume redis`). Each
gets a volume of its own for that repository, on every runtime, which survives
its sessions: the services stop cleanly when the sandbox ends and their data is
taken back then. With two sessions on the repository at once, the one that ends
last is what the next session finds. The agent is told which it has (the
`data` field of `service_start`'s answer), and nothing it does removes one.
You reset it, and only while no session holds it: **Settings → System →
Service data** or `tracon repo reset-volume github.com/owner/app postgres`. The
data pane counts the volumes as Service data, and a sweep (`tracon gc`, the
`[gc]` schedule) removes them with the repository's builds once no entry names
the repository.

## Review sessions

A review can also get a second opinion from a **review session**: a fresh session
that never saw the implementation. It starts from the submitted change with none of
the implementing session's conversation, reads the item and the
diff, and judges the one against the other. Beside the diff it is given what the node
holds about the candidate: the required checks run on it, with the end of their
output, the work shown at that commit, and what earlier reviewers of the same review
found. When there is none it is told so. It runs in the repository's image where its
`[[repo]]` entry names one, so it can run the checks itself, and is told plainly when
it cannot.

It is off until a channel names its model. Pick one under **Settings → Channels →
Review model**, beside Plan and Execute, or bind it from the command line:

```sh
tracon channel bind work phases.review.model=anthropic/claude-opus-5-5
tracon channel bind work phases.review.budget_tokens=400000   # optional; else [session] budget_tokens
```

Whether a submission starts one on its own is set apart from the model, for each
place a submission comes from: a session this node runs, or a harness you run
yourself (which sends only its diff, with no checks or shown work for a reviewer to
go on). Set it beside the review model in **Settings → Channels**, or:

```sh
tracon channel bind work phases.review.trigger.session=auto      # the default
tracon channel bind work phases.review.trigger.external=manual   # the default, unless [external] check_submissions
tracon channel bind work phases.review.trigger=manual            # both at once
```

With `[external] check_submissions` on, an external submission's default is `auto`
instead: the node runs the repository's checks on it, and the reviewer starts once
they are done, with them to go on. A trigger the channel sets still wins.
`[external] check_run` alone does not change it. With `check_submissions` off, a
reviewer started on an external submission runs those checks first, and waits for
them, unless the repository has no `[[repo]]` entry.

Under `auto`, each submission from that source, and each resubmission, starts a
reviewer. Under `manual`, none starts on its own and the review says so; start one
with **Start reviewer** on the review page. The trigger can be set with no model
bound, but then neither starts a reviewer, and the settings row and the review page
say so. The review page names the model under *Reviewed by*, and whether it started
on submission or by you.

The submitting agent is told too: submit_review's message says whether a reviewer
is reading the change, under which trigger, and `review_session` carries its
`trigger` and `model`. While the agent waits, review_status carries `reviewer`
(`state`: starting, running, done, failed or none, with the model), and once the
reviewer has given a verdict on the latest revision, its `verdict`, `summary` and
`findings`. The verdict ends a review_status wait early and is brought back to a
session that ended its turn waiting, so the agent can start on the findings before
you decide; your decision still governs.

When the reviewer requests changes, **Request these changes…** on its verdict opens
the Request changes composer with its summary and findings filled in, one finding a
line. Edit the text or untick a finding, then send it as your own request; the agent
receives it like any other, marked as from the AI review.

A review session never decides. It ends by giving one `review_verdict`, approve or
request changes with a summary and findings, and what it rests on (`ran_checks`,
`cited_evidence` or `diff_only`), which is shown on the review card;
only your decision publishes or sends the work back. Its tools read and run, never
publish: `recall`, `doc_read`, `doc_search`, `work_list`, `work_show`,
`service_start`, `service_status`, `show_work`, `stash_get`, `stash_put`, and
`review_verdict`. Where the
change it reviews is already open on the forge, it can also turn that change
back into a draft with `pr_ready` or `mr_ready` (`ready: false`) when it finds
something blocking, so nobody merges it in the meantime.

A pull request that should not be merged right now is kept a draft. The session
that owns a review and its reviewer turn that review's change back into a draft
with `pr_ready` or `mr_ready` (`ready: false`) without being asked, once a
publication of the review has pushed to it or opened it. An external harness may
do the same, unasked, for a review submitted through it on its channel.
Marking it ready, and anything on another change, is still yours to allow.
Submitting a new revision of a review whose change is open drafts it at once,
and approving a revision marks it ready again. A change an authority grant
publishes without you opens as a draft, and `pr_merge` and `mr_merge` refuse a
draft until it is marked ready. A `pr_merge` whose `head_sha` is not the pull
request's current head is refused before you are asked, and its card names the
head it merges in full.

## Sending a screenshot or a log

The session's message box takes files as well as text, in the browser and on the
phone: **Attach** opens the file picker (on a phone, the camera and the photo
library too), and an image pasted into the box is attached rather than pasted. The
prompt API carries them beside the text: `POST /api/sessions/{id}/prompt` (and
`/ask`, for an ended session asked again) takes `attachments`, a list of
`{name, type, data}` with the bytes base64 in `data`, and answers with what it
staged.

Before the prompt is sent, the node stages each file into the session's workspace,
under one fixed path, `.tracon/attachments/<id>/<name>`, on every runtime (Podman,
Apple, Kubernetes, DigitalOcean): into the running sandbox's own tree over the files
channel, or into the node's copy of the workspace that the sandbox is sent when it
starts. The write follows the files channel's rules — plain files only, nothing
written through a link left in the workspace — so nothing lands outside that
directory, and its `.gitignore` keeps attachments out of every change the session
submits. The prompt the agent gets ends with a line for each file, naming its path,
its type and its size, and the session's log records it as `attachments_staged`:
each file's name, path, type, size and SHA-256. A prompt held while a turn runs is
staged when you send it, so its files are there for the turn that delivers it.

The node enforces the limits and refuses the whole prompt, saying which file and
why, rather than dropping one:

| Limit | Value |
| --- | --- |
| Files per prompt | 8 |
| One file | 10 MiB |
| All of a prompt's files | 20 MiB |
| Types | PNG, JPEG, GIF or WebP images, PDF, or UTF-8 text (logs, JSON, CSV, Markdown, source) |

The type is read from the bytes, not from the name or what the browser says; a name
is cut to its last component, at most 128 bytes, and two files of one name are kept
as `name`, `name-2`. The message box checks the same limits before it uploads
anything and says why a file was left out. Attachments are staged by the node that
runs the session: a peer's session, or an external agent working in its own
checkout, refuses them with that reason.

## Asking, pinging, and complaining

Three tools let an agent reach you without pretending a question is a permission.
`ask_operator` posts a free-text question, optionally with choices, and waits up
to 45 seconds for you to answer it from the queue or the session. You can always
write a note, beside a choice or instead of one, so the answer is `{choice, text}`
with either part optional. Past that it
returns `unanswered` with a `question_id`, and `question_status` carries the wait
in 45-second calls, so no client gives up on the call first. The question
survives the call returning, the client disconnecting, or the node restarting:
an answer given meanwhile is kept for the next status call, and a retry with the
same `request_id` picks it up instead of asking twice. The agent need not keep
polling, either: a session whose turn ends while a question, a review, or a call
it asked you to allow is still undecided is held as *waiting on you* instead of
ending (a plan session that has written its plan included), and your decision
reaches it as its next prompt. Silence is
not consent, and an answer widens nothing. `notify_operator` asks for a real
push to your devices — title, message, and a link back — rate-limited and
deduplicated, and reports only what the push service said, never that you saw
it. `report_issue` drafts a bug report against tracon itself, with expected and
actual behaviour, reproduction, versions, and attachments, secrets scrubbed; the
draft sits in the queue for you to read before you authorize opening it on
GitHub through the broker, or discard it with an optional reason. Its limits are
counted in characters and stated in the tool's schema: a title of at most 256,
and a published body (every field, its headings and the attachments) of at
most 60,000, which any one field may use, under GitHub's 65,536. The agent
follows its draft with `issue_report_status`, which waits up to 45 seconds and
returns the published issue's number, URL and title, or that you discarded it
and why. None of the three pauses the session.

**Access & devices** manages this browser's subscription and device tests.
Shared channel notification rules live under **Channels & repositories** and have an explicit
mesh scope. Push-service acceptance is not phone display or human acknowledgement.
No registered devices means no device delivery can be demonstrated.

## Pause and stop

**Pause** and **Stop** work differently depending on who is driving the harness.
For a session the node itself launched, pause waits for the turn in flight to
finish, then blocks new prompts and permission replies until you resume it —
stop tears the container down outright, and both survive a node restart because
the fence is durable state, not an in-memory flag. A harness you run yourself
over `tracon external` has no session here: the node never held its process, so
the only control is the channel's **Stop broker access**, which refuses every
external agent's calls on that channel until you start it again from
**Settings → Your own harness** or with `tracon external start <channel>`
(`tracon external stop <channel>` stops it). Your own client keeps running until
you end it. A watchdog pauses a session on its own after repeated harness-turn
failures, with the reason on the record, rather than restarting the same loop
indefinitely.
