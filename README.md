# tracon

Personal agent orchestration. A supervisor and control plane that sits between a human
and existing coding agents, driven from a browser instead of a terminal.

Named for TRACON, terminal radar approach control: the facility sequences traffic and
issues clearances, and it never flies anything.

**Status: Phase 1 complete.** A task can be driven from the browser against one macOS
host with a Podman machine: the node establishes and verifies its boundary, spawns `omp`
inside it, routes permission requests to a queue you answer, enforces a token budget, and
streams the session to an embedded interface, on the desktop and at phone width. Credentials live in a broker the harness
cannot read and are reached as tools rather than held as secrets; `consulta` is the first.
Policy decides what is answered without asking and what is refused with a reason; the five
working agreements ship as its starting bundle. What is not built yet is the mesh, the
memory and document corpus, the work ledger, and the clients — Phases 2 through 6. Phase 0 validation is
complete; the work Coder template cannot enforce the gate as configured, so Phase 3
requires a new, unprivileged runner topology.

See [docs/ROADMAP.md](docs/ROADMAP.md) for phasing,
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design record,
[docs/DESIGN.md](docs/DESIGN.md) for the interface, and
[docs/reference/phase-1-spikes.md](docs/reference/phase-1-spikes.md) for what the
implementation learned that the design did not predict.

## What it is

A single statically linked Rust binary that can run as a node on any machine able to
establish the harness boundary: laptops, replacement Coder runners, or homelab
Kubernetes pods. Each node supervises local agent harnesses, enforces policy, brokers
credentials and tools, and serves an embedded SPA. Nodes dial out to an always-on hub
that relays end-to-end encrypted frames, so any node's interface can see and control
work happening on any other.

## What it is not

It is not a coding agent. It drives omp, opencode, and Claude Code over their existing
protocols and contains no model loop of its own. Harnesses are meant to be replaceable.

It is not a work management app. Clients, invoicing, and time billing stay in Kritee.

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
4. **No visibility when the laptops are closed.** Homelab pods stay awake. Nothing today
   lets a phone reach them.

## Shape

```
                        ┌──────────────────────┐
                        │  hub (homelab)       │
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

## Repos this replaces

| Repo | Disposition |
|---|---|
| `review` | Contract absorbed; the broker makes it enforcing rather than advisory |
| `consulta` | Absorbed as node MCP tools (`query`, `describe`); its DB credential is the first thing the broker holds |
| `notebook` | Document corpus absorbed; the `<type>-<slug>` prefix scheme is kept |
| `switchboard` | Superseded by the desktop wrapper; retired last |

`pager` is kept and becomes the notification sink for personal and client channels.
`svastha` contributes the relay and trust-binding patterns. `homelab` hosts the hub.
`kritee` is unaffected.

## Running the node

Requires a container runtime the node can own (rootless Podman; a Podman machine on
macOS), `just`, a Rust toolchain, and Bun for the interface.

```sh
just build      # build the SPA, then the release binary
just setup      # build the gateway and harness images, the internal network, allowlist, and gateway
just boundary   # verify the boundary, including an egress probe and the forward from inside it
./target/release/tracon serve
```

On a host that only needs to run a node, skip the toolchain:

```sh
curl -fsSL https://raw.githubusercontent.com/cosmicspork/tracon/main/install.sh | sh
tracon setup                        # the container definitions ship inside the binary
tracon credential import creds.toml # a model credential, or connect a provider from the Nodes screen
tracon check-boundary --deep
tracon serve
```

As a pod (the work topology), the node is an image rather than a binary, and the
boundary is Kubernetes: one harness pod per session, a NetworkPolicy that makes the node
the harness's only route, and the node serving the allowlist proxy itself. The manifests
are in `deploy/kubernetes/base`; `deploy/kubernetes/lab` is the homelab overlay the
topology was proven on.

```sh
kubectl apply -k deploy/kubernetes/lab
kubectl -n tracon-lab exec deploy/tracon-node -- tracon check-boundary --deep
kubectl -n tracon-lab port-forward deploy/tracon-node 7420:7420
```

`tracon check-boundary` prints each check and exits non-zero naming the first failure.
A node that fails refuses to run harnesses; it still serves the interface and says which
check failed. That refusal is the design working, not a bug to route around.

The harness reaches exactly two things: allowlisted provider hosts through the gateway's
proxy, and the node through the gateway's forward. The allowlist is generated from
`[gateway] allow_hosts` in `~/.config/tracon/node.toml`; a provider that is not listed is
denied, which shows up as a model call that cannot connect.

Configuration and state live where the platform puts them, which on macOS is the same
directory for both:

| | macOS | Linux |
|---|---|---|
| `node.toml` | `~/Library/Application Support/tracon/` | `~/.config/tracon/` |
| database, credentials, identity, harness volume, scratch | `~/Library/Application Support/tracon/` | `~/.local/state/tracon/` |
| harness socket | (TCP `127.0.0.1:7421` through the VM) | `$XDG_RUNTIME_DIR/tracon/harness.sock` |

The gateway forwards to the node over TCP on a Podman machine and over that Unix socket
on a Linux host; `[gateway] harness_listen` takes either form.

### The mesh

Nodes see each other through the hub, an always-on relay in the homelab cluster that
routes sealed frames per channel and can read none of them. Every node dials out;
nothing accepts inbound connections. A hub outage costs latency: local sessions
continue, and what was queued is delivered when it returns.

```sh
# the hub, from source (the release ships ghcr.io/cosmicspork/tracon-hub)
TRACON_HUB_ADMIT=<first node id> TRACON_HUB_DATA_DIR=/var/lib/tracon-hub tracon-hub

# the first node
tracon mesh id                              # its id, for TRACON_HUB_ADMIT
tracon mesh init --hub https://tracon-hub.0x69.xyz
tracon channel create personal
tracon serve

# every other node: invite from an enrolled node, accept on the new one
tracon mesh invite --channels personal      # prints a code, a URL, a QR, and this node's fingerprint
tracon enroll https://tracon-hub.0x69.xyz/#enroll=7KQ4M2XA   # on the new machine; prints its fingerprint
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
sealed once and set aside. What the harness gets is a tool it may
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

### Review before publish

An agent has no forge token and never runs `gh` or `glab`. To get something published it
commits, calls `submit_review`, and waits: the node captures the diff from the worktree
itself, so what is reviewed is what the branch contains rather than what the agent says
it contains. A human approves in the queue — editing the title and body first if they
want to — and the node publishes *those* bytes with the brokered credential.

Each file's git blob hash is recorded at submit. If the branch moves afterwards, approval
is refused and the changed files are named: publishing something nobody read is the
failure this prevents.

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
boundary on Bazzite. The current work Coder template's single Envbuilder container is
privileged and grants passwordless `sudo`; it cannot host a gated harness. Phase 3 must
replace that topology before work-side enforcement is possible. Phase 1 is the gate on
any one machine that passes the boundary check; browser-over-TUI is already settled from
using opencode web and Claude Code web, so no phase re-proves it.
