# tracon

Personal agent orchestration. A supervisor and control plane that sits between a human
and existing coding agents, driven from a browser instead of a terminal.

Named for TRACON, terminal radar approach control: the facility sequences traffic and
issues clearances, and it never flies anything.

**Status: Phase 1 in progress.** The node runs gated sessions on one macOS host with a
Podman machine: it establishes and verifies its boundary, spawns `omp` inside it, routes
permission requests to a queue, enforces a token budget, and streams the session over
HTTP. What is not built yet is the interface (the SPA is a placeholder), the credential
broker and its brokered tools, the review contract, and the mesh. Phase 0 validation is
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
just images     # build the gateway and harness images
just setup      # create the internal network, allowlist, and gateway
just boundary   # verify the boundary, including an egress probe from inside it
just build      # build the SPA, then the release binary
./target/release/tracon serve
```

`tracon check-boundary` prints each check and exits non-zero naming the first failure.
A node that fails refuses to run harnesses; it still serves the interface and says which
check failed. That refusal is the design working, not a bug to route around.

The harness reaches exactly two things: allowlisted provider hosts through the gateway's
proxy, and the node through the gateway's forward. The allowlist is generated from
`[gateway] allow_hosts` in `~/.config/tracon/node.toml`; a provider that is not listed is
denied, which shows up as a model call that cannot connect.

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
