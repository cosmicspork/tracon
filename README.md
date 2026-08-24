# tracon

Personal agent orchestration. A supervisor and control plane that sits between a human
and existing coding agents, driven from a browser instead of a terminal.

Named for TRACON, terminal radar approach control: the facility sequences traffic and
issues clearances, and it never flies anything.

**Status: planning.** No implementation yet. See [docs/ROADMAP.md](docs/ROADMAP.md) for phasing,
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design record, and
[docs/VALIDATION.md](docs/VALIDATION.md) for Phase 0 results.

## What it is

A single statically linked Rust binary that runs as a node on every machine where agent
work happens: laptops, Coder workspaces, homelab Kubernetes pods. Each node supervises
local agent harnesses, enforces policy, brokers credentials and tools, and serves an
embedded SPA. Nodes dial out to an always-on hub that relays end-to-end encrypted
frames, so any node's interface can see and control work happening on any other.

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
                 ┌─────────────────┼─────────────────┐
           ┌─────▼──────┐   ┌──────▼─────┐   ┌───────▼────┐
           │ work node  │   │ personal   │   │  PWA /     │
           │ (Coder pod)│   │ node       │   │  desktop   │
           └─────┬──────┘   └────────────┘   └────────────┘
                 │ docker exec
           ┌─────▼──────────┐
           │  devcontainer  │
           │  (harness)     │
           └────────────────┘
```

Nothing accepts inbound connections. The hub is also the relay; work channels pass
through it as ciphertext it cannot read. The harness runs one privilege boundary below
the node, which is what makes the credential gate real rather than advisory.

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

## Reading order

1. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for decisions and rationale
2. [docs/ROADMAP.md](docs/ROADMAP.md) for what gets built when, and what is deliberately
   deferred or excluded
3. [docs/DESIGN.md](docs/DESIGN.md) for the interface: principles, jobs, states, flows
4. [docs/VALIDATION.md](docs/VALIDATION.md) for what Phase 0 proved and what is still
   pending on the work side

Phase 0 of the roadmap is validation, not code. Several assumptions in the architecture
are load-bearing and cheap to check, and at least one of them (whether the work-side
devcontainer exposes a Docker socket) determines whether the gate can be enforced at all
on that machine. Phase 1 is the gate on one machine; browser-over-TUI is already
settled from using opencode web and Claude Code web, so no phase re-proves it.
