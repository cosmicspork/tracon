# Phase 5 notes: ledger, phases, metrics

Evidence and findings from building Phase 5. The plan's decisions (2026-08-28): the
operator starts plan and execute sessions and the node spawns the review session; checks
run in a throwaway harness container, never on the node host; the reviewer's security
patch is applied as delivered, with revocation widened to the admitting node; work items
replicate through the `sync` crate.

## The security patch (#52)

A reviewer delivered a patch against `16943f5` that applied cleanly to `main`. What it
changed, kept as delivered: a change's channel is the sealed envelope's, never the row
JSON, and an id cannot move between channels; an unrepresentable remote HLC stamp is
`Malformed` rather than poisoning the local clock (counter overflow borrows a
millisecond); the document `If-Match` check and the write are one transaction (a threaded
test proves the race); `If-None-Match: *` is a create-only PUT, which the editor sends
for a new document; the SPA escapes embedded HTML and drops non-http(s)/mailto URLs in
markdown; CSP / nosniff / no-referrer on the interface; the permission card shows the
full request; a third-party `/v0/admit` grant can no longer rotate the target's key or
name. The reviewer could not run Rust: two clippy nits (boxing `DocumentWrite::Written`,
the argument count on `write_document`) were the only changes.

One hunk was widened rather than kept: the patch made `DELETE /v0/admit/{id}` self-only,
which left no API path to revoke a lost node. It now allows self, the node recorded as
`admitted_by`, or the hub; `tracon mesh remove <node-id>` is the CLI. A peer that
neither admitted a node nor is that node still cannot evict it.

## Ids and order

`sha256("tracon/work-item" ‖ 0x1f ‖ channel ‖ 0x1f ‖ project ‖ 0x1f ‖ site ‖ 0x1f ‖
created_ms ‖ 0x1f ‖ title)`, with the site in the preimage so two nodes minting during a
hub outage cannot collide. Readiness is derived, never stored: a Kahn pass over the open
subgraph (ids break ties), then `(priority desc, created_ms asc, id asc)`. Unknown deps
block and are shown as "not seen on this node" rather than silently ready; a cycle blocks
every member. The permutation test in `sync/src/work.rs` is the guarantee that every
replica lists the same order. An item a live session holds is "in session": still ready
by the graph, not offered.

## Phases

`NewSession.phase` defaults to `execute` so older peers' `Command::Create` still parse.
The ledger check runs after the node-ready and version checks, so a refused node still
says why it is refused. The plan artifact is `plan-<first 12 of the item id>`: a plan
session's `doc_write` to that one slug skips the policy gate (the session exists to write
it), sets `phase_plan_slug` on the item, records `plan_artifact`, and ends the session
with `EndReason::PhaseDone` once the turn is over — `Command::EndAfterTurn(reason)`, which
ends immediately when no turn is running. The same command carries `ItemClose` from
`work_close`, from a close in the interface, and from a publish.

## Checks

`Runner::run_capture` with `sh -lc <command>`, workdir `/work`, the worktree as the only
mount, no env. The local runner used by tests now honours a workdir that names a mount
target by running in that mount's source, so `node/tests/review.rs` runs real commands
against a real worktree (`test -f a.txt`, `sh -c 'echo boom >&2; exit 3'`). Checks stop at
the first failure; the tail is the last 4 KiB of stdout+stderr. A missing `just` recipe
is a failing check, reported, not skipped. Container start time on Podman is a cost paid
per submit; not measured here because the in-process tests use the local runner — record
it on the first real run.

## Review sessions

The review session's worktree is `git worktree add -b review/<id> <head_sha>`: the
reviewed commit is already in the shared object store, nothing is fetched. Its tools are
narrowed by phase in `Tools::list_for` and enforced in `Tools::call` (a call to anything
else is refused by name, before the policy gate). `review_verdict` needs
`phases.review.model` bound on the channel; without it the submit reply says "no review
model bound" and the card says the same. The verdict is `ai_verdict_json` on the review
row; it is never a decision.

## Ceiling and metrics

The ceiling counts gateway tokens (`model_usage`, input + output) since local midnight
(`TRACON_TZ_OFFSET_MINUTES`, as the nightly batch). Two enforcement points: `create`
(429) and the gateway (429 per call, one `ceiling` event per session). The harness's own
`tokens_used` stays the per-session meter. Metrics are computed on the node: human time on
reviews is claim→`updated_ms` at decision (wall clock on the deciding node) because the
row keeps no monotonic resolve stamp; permission latency is monotonic. Cost is per
provider price, optional, so a subscription stays "unpriced" rather than a made-up number.

## Interface

Built to a single mockup page in the tokens, then checked with headless Chromium at 1280
and 390 wide against a seeded scratch node. One thing the mockup got wrong and the build
fixed: `/metrics` had to be matched before the Nodes branch of the router. "Waiting on a
check" is the accent, not amber — it waits on the node.
