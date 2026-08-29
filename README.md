# tracon

Personal agent orchestration. A supervisor and control plane that sits between a human
and existing coding agents, driven from a browser instead of a terminal.

Named for TRACON, terminal radar approach control: the facility sequences traffic and
issues clearances, and it never flies anything.

**Status: Phases 1 through 6 built.** A task can be driven from a browser, or from a
phone, against a node that establishes and verifies its own boundary, spawns a harness
inside it, routes permission requests to a queue you answer, enforces a token budget,
and streams the session to an embedded interface. Nodes replicate through a hub that
cannot read a work channel. The corpus keeps memories and documents and is searched by
text, and by meaning where a node is given an embedding endpoint. The ledger carries
work from plan to review, and a review is approved, refused, or edited and sent back as
a patch. Clients are the installable interface, notifications bound per channel, and a
tray wrapper. Credentials live in a broker the harness cannot read and are reached as
tools rather than held as secrets. Policy decides what is answered without asking and
what is refused with a reason.

Two harnesses have adapters: `omp` over ACP, and Claude Code over its stream-json
control protocol. A node runs one, chosen with `[harness] id` and pinned by
`[harness] version`; the pin is checked twice and an unknown id refuses to start. What is not built is listed honestly at the end of each phase in
[docs/ROADMAP.md](docs/ROADMAP.md); the ones worth knowing here are that the desktop
wrapper is unsigned and hub-side rollups do not exist.

See [docs/ROADMAP.md](docs/ROADMAP.md) for phasing,
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design record,
[docs/DESIGN.md](docs/DESIGN.md) for the interface, and
[docs/reference/phase-1-spikes.md](docs/reference/phase-1-spikes.md) for what the
implementation learned that the design did not predict.

## What it is

A single statically linked Rust binary that can run as a node on any machine able to
establish the harness boundary: laptops, replacement Coder runners, or always-on
Kubernetes pods. Each node supervises local agent harnesses, enforces policy, brokers
credentials and tools, and serves an embedded SPA. Nodes dial out to an always-on hub
that relays end-to-end encrypted frames, so any node's interface can see and control
work happening on any other.

## What it is not

It is not a coding agent. It drives omp, opencode, and Claude Code over their existing
protocols and contains no model loop of its own. Harnesses are meant to be replaceable.

It is not a work management app. Clients, invoicing, and time billing live elsewhere.

It is not multi-user. One operator holds the keys. Channel separation is cryptographic
isolation between that operator's contexts, not tenancy.

## Why

Four problems, in the order they hurt:

1. **Latency.** Driving a TUI in a remote workspace from the other side of the world
   round-trips every keystroke. A chat interface round-trips once per prompt.
2. **Environment sprawl.** Agents running locally against remote environments mean
   spinning up devcontainers on a laptop just to run tests that belong where the code
   already lives.
3. **Unenforced rules.** Working agreements live in a markdown file the agent may have
   forgotten by hour two. Worktree not main checkout, review before publish, no merge,
   no transition, no production deploy. All enforceable, none enforced.
4. **No visibility when the laptops are closed.** The cluster's pods stay awake. Nothing
   today lets a phone reach them.

## Shape

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

Nothing accepts inbound connections. The hub is also the relay; work channels pass
through it as ciphertext it cannot read. A host runs harnesses only after proving that
the isolated runner is one privilege boundary below the node. That boundary makes the
credential gate real rather than advisory.

## Design commitments

These constrain everything else and are unlikely to change.

- **The node supervises, it does not reason.** No agent loop.
- **Enforcement over instruction.** A rule the node cannot enforce is a suggestion, and
  should be labeled as one.
- **Local-first.** Hub outage degrades the system. It does not stop work.
- **Nothing in project repos.** No `.claude/`, no `AGENTS.md`, no tracon files of
  any kind. Config is materialized into scratch directories at session start.
- **The corpus outlives the tooling.** Boring schemas, plain-text export. When the
  orchestration layer becomes obsolete, the memory, documents, and work ledger survive
  it.

## Where it came from

tracon grew out of a handful of personal tools: a review contract that was advisory and
is enforcing here, a database helper that became the `consulta` MCP tools, a markdown
notes app whose `<type>-<slug>` prefix scheme the corpus kept, and an end-to-end-encrypted
relay whose trust binding the hub reuses. Phone push was once an external bridge and is
now built in.

## Install

Prebuilt binaries ship for Linux x86_64 (static, runs on any distribution) and macOS on
Apple Silicon. One line fetches the right one, verifies it against the release's
checksums, and puts it in `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/cosmicspork/tracon/main/install.sh | sh
```

`TRACON_VERSION=v0.6.0` pins a release; `TRACON_BIN_DIR` moves the binary. The script
prints the first-run commands below when it is done.

**The desktop app** is a separate download from the same release — `tracon_<version>_amd64.AppImage`
or `.deb` on Linux, `tracon_<version>_aarch64.dmg` on macOS. It is a tray client that
also runs the node for you, and it carries its own copy of the `tracon` binary, so on a
laptop it is the whole install. It is not signed: macOS wants a right-click → Open the
first time.

**From source**, for anything else or to hack on it: Rust, Bun, and rootless Podman.

```sh
just build      # build the SPA, then the release binary (target/release/tracon)
just setup      # build the gateway and harness images, the internal network, allowlist, and gateway
just boundary   # verify the boundary, including an egress probe and the forward from inside it
./target/release/tracon serve
```

The desktop app needs webkit2gtk and gtk headers, which an immutable host does not
have; `just gui` builds it in a container (the recipe says which one) and produces the
same bundles the release does. `just musl` builds the static Linux binary.

## First run

Everything below is on the machine that runs the node. The binary is `tracon`; the
node is `tracon serve`, which does not daemonize, so the last step gives it something to
run under.

```sh
tracon setup                        # the harness network and gateway; the container definitions ship inside the binary
tracon check-boundary --deep        # prove the boundary: every check, plus an egress probe from inside it
tracon service install              # run it under systemd or launchd …
tracon-wrapper                      # … or let the desktop app run it (tracon service uninstall to switch)
```

Then a model credential, without which sessions cannot start: open the interface
(`http://127.0.0.1:7420`), go to **Nodes**, and connect a provider — the harness's own
login runs on the node and the code is pasted back there. An API key instead is
`tracon credential import creds.toml`. To join an existing mesh rather than start one,
`tracon enroll <invitation url>` first; see [The mesh](#the-mesh).

`tracon check-boundary` prints each check and exits non-zero naming the first failure.
A node that fails refuses to run harnesses; it still serves the interface and says which
check failed. That refusal is the design working, not a bug to route around.

The harness reaches exactly two things: allowlisted provider hosts through the gateway's
proxy, and the node through the gateway's forward. The allowlist is generated from
`[gateway] allow_hosts`; a provider that is not listed is denied, which shows up as a
model call that cannot connect.

### Reaching it from another device

On its own machine the node answers loopback with no ceremony: the CLI, `just dev` and
`kubectl port-forward` all arrive that way. Anything else is refused until a token
exists:

```sh
tracon auth issue          # printed once; exchanged for a cookie at the login screen
tracon auth sessions       # what is logged in
tracon auth revoke         # loopback only again
```

The cookie is `Secure`, so the node has to be behind TLS (an ingress, a reverse proxy)
for a browser to keep it — a plain-HTTP node on the LAN is not a supported way to use
it; `localhost` counts as secure. Issuing a token again rotates it and logs every client
out. `TRACON_TOKEN` lets the CLI reach a remote node (`TRACON_URL` says which).

From a phone: log in, add the interface to the Home Screen (iOS only pushes to an
installed web app), open **Nodes**, and switch on **Push to this device**. See
[Clients](#clients) for what that does.

### As a pod

The node is an image rather than a binary, and the boundary is Kubernetes: one harness
pod per session, a NetworkPolicy that makes the node the harness's only route, and the
node serving the allowlist proxy itself. The manifests are in `deploy/kubernetes/base`,
ready for a kustomize overlay that sets the namespace, the image tag and the storage
class.

```sh
kubectl apply -k deploy/kubernetes/base
kubectl -n <namespace> exec deploy/tracon-node -- tracon check-boundary --deep
kubectl -n <namespace> port-forward deploy/tracon-node 7420:7420
```

### Where things live

Configuration and state live where the platform puts them, which on macOS is the same
directory for both:

| | macOS | Linux |
|---|---|---|
| `node.toml` | `~/Library/Application Support/tracon/` | `~/.config/tracon/` |
| database, credentials, identity, harness volume, scratch | `~/Library/Application Support/tracon/` | `~/.local/state/tracon/` |
| harness socket | (TCP `127.0.0.1:7421` through the VM) | `$XDG_RUNTIME_DIR/tracon/harness.sock` |

`TRACON_STATE_DIR` overrides the state directory outright, which is how a scratch node
is run beside a real one (`XDG_RUNTIME_DIR` must be short — a long path hits the Unix
socket limit and fails as `bind …/harness.sock`). `TRACON_LISTEN` or `serve --listen`
moves the API off `127.0.0.1:7420`.

### Configuration

`node.toml` is optional; every key has a default. The full set, with defaults:

```toml
node_name = "<hostname>"            # how this node is named in the mesh

[harness]
id = "omp"                          # "omp" or "claude"; an unknown id refuses to start
version = "18.0.4"                  # pinned; checked against the image and the host
tools = []                          # extra tool names to offer; empty is everything the harness has

[boundary]                          # the rootless-Podman boundary a laptop establishes
network = "tracon-int"
subnet = "10.89.0.0/24"
gateway_ip = "10.89.0.2"
gateway_container = "tracon-gw"
gateway_image = "localhost/tracon-gateway"
harness_image = "localhost/tracon-harness"
# selinux_label_disable = true      # only if the boundary check says the labels fight you

[gateway]
allow_hosts = ['^api\.anthropic\.com$', '^api\.openai\.com$', '^chatgpt\.com$', '^auth\.openai\.com$']
proxy_port = 8888
forward_port = 7421
# harness_listen = "127.0.0.1:7421" # or a socket path; the platform default is right

[session]
budget_tokens = 2000000             # per session
permission_timeout_secs = 900       # an unanswered ask is a deny
claim_grace_secs = 60               # a review claim lapses this long after the client vanishes
# worktree_root = "/tmp"            # /private/tmp on macOS

[runtime]
kind = "podman"                     # or "kubernetes", for a pod-hosted node
# [runtime.kubernetes]              # namespace, harness_image, state_claim, state_mount, harness_home, uid, gateway_host

[providers.anthropic]               # anthropic and openai are built in; add others the same way
credential = "anthropic"
upstream = "https://api.anthropic.com"
shape = "anthropic"                 # or "openai"
# login = "…"                       # the harness's login flow, if it has one
# [providers.anthropic.price]
# input_per_mtok = 3.0
# output_per_mtok = 15.0

[consulta]                          # the database MCP tools, run as a sidecar
command = "uv"
args = ["run", "--project", "~/src/consulta", "consulta"]
timeout_secs = 60

[publish]                           # the binaries the node runs to publish an approved review
gh = "gh"
glab = "glab"
git = "git"

[mesh]
# hub_url = "https://hub.example.com"   # set by tracon mesh init / enroll
heartbeat_secs = 60
poll_secs = 30
command_timeout_secs = 15

[memory]
promote_at = "02:00"                # the nightly promotion batch

[supervision]
checks = ["just check"]             # run in the worktree before a review is accepted
timeout_secs = 900

[review]
max_diff_lines = 800                # a bigger submission is refused before any check runs
max_files = 40

[notify]
# contact = "mailto:you@example.com"    # what a push service may write to about this sender

[embed]                             # semantic search; off unless enabled
enabled = false
base_url = "http://127.0.0.1:8080"  # an OpenAI-shaped /v1/embeddings
model = "bge-m3"
dim = 1024                          # must match the model; changing it rebuilds the index
# api_key_file = "~/.config/llama-server.key"
# provider = "anthropic"            # instead of base_url: through the gateway, so the channel ceiling applies
batch = 16
timeout_secs = 60
```

For `[embed]`, a local `llama-server --embedding -m <model>.gguf --host 127.0.0.1 --port 8080`
is enough; BGE-M3 and Qwen3-Embedding-0.6B (which wants `pooling = last`) are the models
it was built against. Without an endpoint, search is text-only and the Documents screen
says so.

### Commands

| | |
|---|---|
| `tracon serve [--listen]` | run the node |
| `tracon setup [--rebuild]`, `check-boundary [--deep]` | the boundary |
| `tracon service install\|uninstall\|status` | the platform supervisor |
| `tracon auth issue\|revoke\|sessions` | off-machine access |
| `tracon push ls\|rm <id>\|test` | the phones this node pushes to |
| `tracon mesh id\|init\|invite\|members\|remove\|admit`, `enroll` | the mesh |
| `tracon channel create\|list\|bind\|share` | channels and their bindings |
| `tracon credential import\|ls\|rm\|share` | what the broker holds |
| `tracon doc import\|ls\|get\|put\|rm\|export` | documents |
| `tracon memory ls\|add\|rm\|recall\|batch` | memories, and the promotion batch on demand |
| `tracon work add\|ls\|ready\|show\|close\|dep\|rm` | the ledger |
| `tracon policy keygen\|init\|sign\|push\|show` | the policy bundle |
| `tracon metrics [--channel] [--days]`, `provenance <sha>` | what happened |

Every command talks to the running node over its API; `--help` on any of them says more.

### The hub

The hub is a separate binary, `tracon-hub`, shipped as a static Linux binary and as the
`ghcr.io/cosmicspork/tracon-hub` image. It is configured by environment:

| Variable | Default | |
|---|---|---|
| `TRACON_HUB_ADDR` | `127.0.0.1:8080` | listen address (the image sets `0.0.0.0:8080`) |
| `TRACON_HUB_DATA_DIR` | in memory, with a warning | durable frames, members and the replica |
| `TRACON_HUB_ADMIT` | — | the first node's id, so something can connect |
| `TRACON_HUB_RETAIN_DAYS` | 14 | how long frames are kept |
| `TRACON_HUB_MAX_SKEW_SECS`, `TRACON_HUB_MAX_CHANNEL_BYTES`, `TRACON_HUB_ENROLL_TTL_SECS`, `TRACON_HUB_ENROLL_RATE_PER_MIN` | 300, 256 MiB, 600, — | limits |
| `TRACON_HUB_REPLICA` | on when there is a data dir | the hub's own replica of what it can open |
| `TRACON_HUB_PROMOTE_AT` | `03:00` | the hub-side promotion batch |
| `TRACON_HUB_SNAPSHOT_ENDPOINT`, `_BUCKET`, `_ACCESS_KEY`, `_SECRET_KEY`, `_PREFIX`, `_EVERY_HOURS`, `_KEEP`, `_PUBKEY` | — | encrypted snapshots to S3-compatible storage; `tracon-hub snapshot-key` makes the key |
| `TRACON_HUB_RESTORE_SEED` | — | for `tracon-hub restore` |

## Using it

### The mesh

Nodes see each other through the hub, an always-on relay on a cluster somewhere that
routes sealed frames per channel and can read none of them. Every node dials out;
nothing accepts inbound connections. A hub outage costs latency: local sessions
continue, and what was queued is delivered when it returns.

```sh
# the hub, from source (the release ships ghcr.io/cosmicspork/tracon-hub)
TRACON_HUB_ADMIT=<first node id> TRACON_HUB_DATA_DIR=/var/lib/tracon-hub tracon-hub

# the first node
tracon mesh id                              # its id, for TRACON_HUB_ADMIT
tracon mesh init --hub https://hub.example.com
tracon channel create personal
tracon serve

# every other node: invite from an enrolled node, accept on the new one
tracon mesh invite --channels personal      # prints a code, a URL, a QR, and this node's fingerprint
tracon enroll https://hub.example.com/#enroll=7KQ4M2XA   # on the new machine; prints its fingerprint
```

The inviter shows the new node's name and fingerprint and asks whether it matches what
the other terminal printed; on `y` it admits the node and hands it the channel keys and
the policy bundle, sealed to that node. The Nodes screen does the same thing in the
browser. A channel is a key; a node that was not handed a channel's key cannot read it,
and a meshed node refuses to start a session on a channel it holds no key for.

Policy is signed where it is written and never by the hub: `tracon policy push` hands a
new bundle to every member, and a node installs it only if it is signed by the key it
received at enrollment.

### Brokered tools

Credentials live in `credentials.sealed` under the node's state directory, sealed under a
key derived from the node's identity seed (a copied file is ciphertext elsewhere; losing
the seed loses the store), and are never given to a harness. `tracon credential import
<file>` seals a plaintext TOML like the one below; `tracon credential ls` shows names,
kinds, and bindings, never values; `tracon credential share <name> --to <node>` hands one
to another member, direct-sealed over the hub, and enrollment hands over every credential
whose `nodes` lists the new node. A plaintext `credentials.toml` found at startup is
sealed once and set aside.

### The corpus

Every node keeps documents and memories, replicated through the hub and read locally
always. `tracon doc import <dir>` brings a directory of markdown in (`<kind>-<slug>.md`);
`tracon doc ls|get|put|rm|export` and the Documents screen read and edit them, with the
hash last read as the edit's precondition. Agents get `recall`, `retain`, `doc_search`,
`doc_read`, and `doc_write` (asked); the operator's directives (`tracon memory add`)
rank first in recall and are injected into every session's orientation. What an agent
retains as a lesson waits for the nightly batch, which arrives on the queue for a
per-item verdict. `tracon channel share --hub <channel>` lets the hub's replica index a
channel and run its batch; a channel never shared stays ciphertext to it.

Search is full-text by default, and by meaning as well on a node given an embedding
endpoint:

```toml
[embed]
enabled = true
base_url = "http://127.0.0.1:8080"   # llama-server --embedding, or any /v1/embeddings
model = "bge-m3"
dim = 1024
```

The endpoint is named rather than linked in, which is what keeps a work channel's corpus
on the work machine. The index is this node's own and never replicates: a vector is not a
safe form of encrypted content, so the hub is never handed a readable index of a channel
it cannot open. It is derived state — delete it and it rebuilds. Where a node embeds but
cannot reach its endpoint, a search says `text only` rather than quietly returning less.

Model credentials are brokered the same way. The harness holds only a placeholder key
(its session token) and reaches every provider through the node's gateway
(`/model/<provider>/…` on the harness forward), which injects the real credential,
enforces the channel's provider bindings, and counts usage (`GET /api/usage`). A
subscription is connected from the Nodes screen: the node runs the harness's own login
inside the boundary, shows the sign-in link, takes the paste-back, and lifts the token
into the store; refresh runs the same way ahead of expiry. An API key is a credential
of kind `api_key` with `provider` set, imported like any other. What the harness gets is a tool it may
ask the node to run, over MCP through the gateway's forward, with a token minted for its
session. A credential names the channels allowed to use it; a channel with none bound is
offered no tools at all. It may also name the nodes allowed to use it, by node id, so a
store copied to another machine brokers nothing there ("consulta on the work node only").

Every tool call is decided by the policy bundle before the broker is touched, under the
kind `tool`: a rule that names the tool allows it, a deny rule returns its reason, and a
tool the bundle does not mention is put to the operator on the session's queue. Adding a
tool never widens what runs unattended.

```toml
# a plaintext file for `tracon credential import`, chmod 600
[credentials.consulta]
channels = ["work"]
nodes = ["2b310f36c18605ac9ab367ec3abe4fe9aaa6aee04e64db98b0ea364e5e6b3013"]

[credentials.consulta.env]
DB_BACKEND = "oracle"
DB_HOST = "…"
DB_PASSWORD = "…"
```

### The ledger

Work is a replicated ledger per channel: items with priorities and dependencies, ids that
two nodes mint offline without colliding, and a ready-work order every node computes
the same way. `tracon work add|ls|ready|show|close|dep|rm` and the Work screen keep it;
an agent gets `work_ready`, `work_discover` (what it found, linked to its item), and
`work_close`. A session is a phase of one item: a **plan** session ends by writing
`plan-<item>`; an **execute** session is refused until that document exists, does the
work, and submits; at submit the node runs the project's checks (`.tracon/checks` in the
worktree, else `[supervision] checks`) in a throwaway container and refuses a failure or
a diff over `[review] max_diff_lines` / `max_files`; then, if the channel binds
`phases.review.model`, a fresh **review** session reads only the requirements and the
diff and leaves its verdict on the card. Approving publishes, closes the item, and ends
the session. `tracon channel bind <name> key=value…` sets the bindings (`phases.*`,
`ceiling_tokens_per_day`) and hands them to every member.

Cost is enforced, not watched: a channel at its daily ceiling starts no session and the
gateway refuses its model calls. `tracon metrics` prints approvals and tokens per
accepted change per channel (dollars only where `[providers.<p>.price]` is set);
`tracon provenance <sha>` answers which model, which prompts, which approval, and which
policy version shipped a commit.

### Clients

Every node serves the same interface, so a client is a matter of shell. It installs as a
PWA — manifest, icons, and a service worker that caches the shell and never caches the
API, because a stale queue is worse than an honest "cannot reach the node". A desktop
wrapper (`wrapper/`, its own cargo workspace) adds what is actually wanted from native:
a tray showing what is waiting, a global hotkey, command-tab presence, and system
notifications. It holds no session state — the window is the interface the node serves,
so a crash there is a reconnect and never lost work.

Something has to run the node, which does not daemonize: the platform's supervisor
(`tracon service install`) or the desktop app, which starts one on launch and stops it
on quit, and **adopts** a node that is already answering rather than starting a second —
two nodes over one state directory would fight over the same database and harness
socket. A machine that must stay reachable through logout keeps the unit; running both
is not a mistake you can make. Off its own machine the node wants a token; see
[Reaching it from another device](#reaching-it-from-another-device).

What is waiting can be pushed to where you are. On the phone, open the Nodes screen and
switch on **Push to this device**; the node pushes straight to the phone's push service
(Web Push, VAPID, sealed to the phone's own key — nothing sits in between). Every node
pushes to the phones subscribed at it, so a phone subscribes wherever it logs in; a
phone reached by two nodes sees each approval once, because the banner's tag is the
same from both. Whether a channel notifies at all is a binding:

```bash
tracon channel bind work notify.enabled=false   # the desktop tray is enough for work
tracon push ls                                  # the devices this node pushes to
tracon push test                                # "notifications are on" to each of them
```

The desktop app shows the same items in its tray regardless. `[notify] contact` in
`node.toml` is the address a push service may write to about this sender (a `mailto:`;
Apple checks its shape).

### Review before publish

An agent has no forge token and never runs `gh` or `glab`. To get something published it
commits, calls `submit_review`, and waits: the node captures the diff from the worktree
itself, so what is reviewed is what the branch contains rather than what the agent says
it contains. A human approves in the queue — editing the title and body first if they
want to — and the node publishes *those* bytes with the brokered credential.

Each file's git blob hash is recorded at submit. If the branch moves afterwards, approval
is refused and the changed files are named: publishing something nobody read is the
failure this prevents.

On a desktop the diff can be edited rather than described: each reviewed file opens as a
unified merge view, and what leaves is a patch. It goes back with the notes, the agent
applies it and resubmits — so an edit is a request for changes, and the agent is still
the only thing that writes to the worktree.

```toml
[credentials.glab]
channels = ["work"]
[credentials.glab.env]
GITLAB_TOKEN = "…"
```

`consulta` is the first, and read-only by construction: the node parses the SQL and
refuses anything that is not a single `SELECT`/`WITH` before spawning anything, and
consulta's own guard refuses again inside the sidecar. Two checks, on opposite sides of
the privilege boundary.

GitLab and Jira are the next two, as the verbs an agent needs and nothing else:
`mr_status`, `mr_comment`, `issue`, `issue_comment`. Merge, transition, and deploy are
not tools. Both talk REST from the node, so no `glab` or `acli` binary is needed where
it runs.

```toml
[credentials.jira]
channels = ["work"]
nodes = ["<work node id>"]
[credentials.jira.env]
JIRA_URL = "https://example.atlassian.net"
JIRA_EMAIL = "…"
JIRA_TOKEN = "…"
```

### Policy

Policy decides what the node answers on its own. Reading is allowed without interrupting
you; the five working agreements are refused with a reason the agent can read; everything
else reaches the queue.

```sh
tracon policy keygen   # the signing key stays here and never reaches the hub
tracon policy init     # write the working agreements as a bundle, and sign it
tracon policy show     # verify and print what it decides
```

Edit `policy.toml` and run `tracon policy sign` again. A bundle that is missing,
malformed, or badly signed yields no rules — and no rules means every request is asked.
The failure mode of broken policy is more questions, never fewer.

Denials are not the absence of an allow rule. Refusing `gh pr merge` with "opening the
change is yours; merging it is the operator's" is what makes the agent stop, rather than
meet a confusing auth error and look for another way round.

### Bootstrap escape hatch

The node is developed using agents that run inside the node, so a bad build must not
lock the operator out of the tool needed to fix it. The path out is to run the harness
directly, outside tracon:

```sh
omp                                    # the harness, unsupervised, in any checkout
git worktree add /private/tmp/<slug> -b <branch> origin/main
```

Nothing in tracon is required to do this, and rebuilding or restarting the node is a
host-side recipe rather than something a session performs.

## Reading order

1. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for decisions and rationale
2. [docs/ROADMAP.md](docs/ROADMAP.md) for what gets built when, and what is deliberately
   deferred or excluded
3. [docs/DESIGN.md](docs/DESIGN.md) for the interface: principles, jobs, states, flows

Phase 0 established the ACP message shapes, restricted-harness behavior, and one working
boundary on an immutable Fedora host with rootless Podman. The work Coder template's single Envbuilder container was privileged
and granted passwordless `sudo`, so it could not host a gated harness; Phase 3 replaced
that topology with one harness pod per session under a NetworkPolicy, which is what made
work-side enforcement possible. Browser-over-TUI was settled before Phase 1 from using
opencode web and Claude Code web, so no phase re-proves it.
