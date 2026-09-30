# OpenCode migration record

The gate-by-gate record of the 2026-09 move from omp to OpenCode, kept here as history.
The roadmap carries only open work; `opencode-v1.18.30/` is the compatibility inventory
and upgrade checklist for the pinned release.

**Decided 2026-09-13, cut over 2026-09-14.** OpenCode is the primary managed harness,
driven through its native server API, with its native web UI offered as an optional
advanced view. Claude Code is retained as a second supported harness. omp is gone — adapter,
image, ACP layer, provider wiring and catalogue workarounds — and its sessions are
archived read-only. No new agent loop was added; the isolated execution boundary is
unchanged.

Pinned candidate: OpenCode v1.18.30, inventoried in `docs/reference/opencode-v1.18.30/`
— compatibility manifest and twenty numbered findings, route/provider/config matrices,
the minimum launch environment, the gateway deny list, and the native UI route trace.
That directory is the history and the upgrade checklist; this is the record of what
each gate delivered.

- **Gate A — candidate inventory and contract** (#189). Release pinned with checksums,
  API snapshot, server flags and startup egress verified live, provider/mutation/config
  matrices produced. Verdict: no stop condition.
- **Gate B — owner-side policy, credentials, convergence** (#190, #193, #195–#200). One
  isolated `opencode serve` per session under a sealed launch environment (#193); the
  policy-aware API gateway — deny by default from the route matrix, directory pinned and
  bodies inspected, an all-`ask` ruleset with every `always` rewritten to `once` and the
  broadening recorded (#195); durable identity mapping and idempotent ingestion anchored
  on the per-session sequenced streams, with uncertain mutations settled by reconciliation
  (#196); per-turn usage reconciled between the gateway's on-the-wire count and the
  harness's own report, unmetered turns flagged rather than charged zero, unsent prompts
  held on the node (#197); the adversarial run and per-session state isolation behind a
  node-owned single-writer fence (#198); provider traffic proven to reach the gateway and
  only the gateway, with finding 19 — the session runner resolving a provider from the
  catalogue rather than `options.baseURL` — found and closed (#199, #200); Anthropic
  subscription login lifted out of `claude setup-token` (#190).
- **Gate C — customization and recovery** (#201–#204, #216). The node-owned launch
  manifest for skills, instructions, agents and approved plugins, with a digest recorded
  on every session (#203); LSP, formatters and the plugin cache baked into the image,
  runner egress rejected, orphaned children reaped (#202); quiesced `VACUUM INTO` backups,
  migration on a clone, generation-gated restore (#204); the direct-harness recovery route
  documented and corpus export proven to round-trip with rebuildable vectors (#201); the
  legacy transition — `tracon session archive-legacy` and `reopen` — with the cutover
  (#216).
- **Gate D — browser, desktop, and installed mobile PWA** (#205–#207, #210–#212, #217).
  The native UI served from its own origin behind a single-use bootstrap, with no password
  in the browser (#206); an unprivileged desktop window navigable only to that origin
  (#205); PTY as an explicit terminal capability with owner-bound tickets and a bounded
  proxy (#207); the installed app hosting the native view in an in-scope frame on a
  partitioned cookie (#210); the 60-shape route trace, the proof that unknown mutations
  fail closed, and the installable pinned bundle (#211, #212); and finding 20's fix — a
  node-synthesised, session-scoped, replayable `/global/event`, which is what makes the
  native view live (#217).
- **Gate E — remote-node parity** (#215). Bounded encrypted owner streams over the hub for
  HTTP, SSE and PTY: authenticated stream ids, owner binding and epoch fencing, flow
  control, reconnect without replaying input, revocation, protocol mismatch refused; the
  hub sees ciphertext and routing metadata only.
- **Gate F — release and clean cutover** (#216, #218, and this change). omp retired;
  README, architecture and design reconciled with the landed migration; the migration plan
  archived. A QA target gained a second deployment kind so the first real run need not be
  GitLab-shaped: `command` runs an operator-configured argv on the node with a brokered
  credential as environment — never in argv, never in the recorded tail — observes the host
  through further argv, and ends in the same identity check and immutable browser binding,
  so browser verification is unchanged (#218). The real Podman and Kubernetes project
  workflows on both harnesses remain on the checklist below.

Alongside the gates: the macOS bundle ships unsigned (#192),
administration and first-task workflows unified (#208), and the provider callback tests
de-raced (#194).
