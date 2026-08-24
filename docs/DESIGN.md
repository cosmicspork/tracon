# Design

Interface design record for tracon. Companion to `ARCHITECTURE.md` (what the system
does) and `ROADMAP.md` (when). This file covers what the operator sees and how the
interface decides things. Steps 1 through 4 of the design ladder are here; wireframes,
tokens, and components arrive with Phase 1.

## 1. Interface principles

Derived from the system principles. Each one is kept only because it changes a real
decision; the decision it changes is listed with it.

1. **Waiting-on-you comes first.** The interface exists to direct work, so the home
   screen on every surface is the queue of things blocked on the operator, not a list
   of sessions. Running output is secondary and never displaces an approval.
   *Decides:* home screen, sort order, what a notification opens to.

2. **Enforcement is visible.** Every approval shows the node it came from, and a node
   that has refused to run harnesses (boundary check failed) says so wherever it would
   otherwise offer work. No screen implies a session is gated when the node could not
   gate it, and no node runs ungated.
   *Decides:* the node chip is part of the approval card, not a separate page; a refused
   node shows its failed check on the nodes screen and in the new-session form.

3. **Degraded is a state, not an error.** Hub unreachable is expected and the interface
   says what still works: local sessions continue, auto-allowed actions proceed,
   approvals queue locally and are answered when the hub returns. No error toasts for
   an expected condition.
   *Decides:* a persistent hub banner instead of modal errors; controls that need the
   hub are disabled with a reason, not hidden.

4. **Read, decide, send.** The phone directs; it does not edit. Anything needing a
   keyboard and a wide screen is desktop-only and says so where it would otherwise
   appear. Capability is gated by surface, not by screen width.
   *Decides:* the diff editor is never rendered on the phone; the verdict control shows
   "edit on desktop" rather than a disabled button with no explanation.

5. **The diff is the unit of review.** Not the file tree, not the repo, not the session
   transcript. Review means reading a diff against its requirements and returning a
   verdict. Editing means editing that diff and submitting it as `/revise`.
   *Decides:* no file browser; the diff viewer is the most invested component; the
   review session's requirements are shown beside the diff, not the implementation
   transcript.

6. **Nothing is lost when the client dies.** Sessions live in the node, so the interface
   never holds state the node does not have. Unsent prompt drafts are held by the node
   per session; diff edits are the one desktop-only exception and sit in local storage
   until submitted.
   *Decides:* reconnect is silent and resumes where the node is, draft prompt included.

## 2. Jobs and surfaces

Every job the operator needs, and which surface it must work on. "Read" means the
surface shows it but cannot act. Phase is when the job first exists.

| Job | Browser | Tray | Phone | Phase |
|---|---|---|---|---|
| See what is waiting on me, across all nodes | yes | yes | yes | 1 |
| Answer a harness permission request | yes | yes | yes | 1 |
| Read a diff and return approve / reject | yes | yes | yes | 1 |
| Edit a diff and submit it as `/revise` | yes | no | no | 6 |
| Start a session (model required, budget) | yes | no | minimal | 1 |
| Send a prompt into a running session | yes | no | yes | 1 |
| Read session output as it streams | yes | no | yes | 1 |
| Kill a session (confirm on tray and phone) | yes | yes | yes | 1 |
| See node capability and hub reachability | yes | yes | yes | 1 / 2 |
| Enroll a new node | yes | no | no | 2 |
| Run a brokered query against the work DB | no (agent-only) | no | no | 1 |
| Read a document by slug | yes | no | yes | 4 |
| Review the nightly memory-promotion batch | yes | no | read + verdict | 4 |
| Browse ready work; follow `discovered-from` | yes | no | read | 5 |
| Pick a work item when starting a session | yes | no | minimal | 5 |
| See today's cost per channel against the ceiling | yes | yes | yes | 5 |

What falls out:

- The **queue** is the only screen all three surfaces need in Phase 1. It is designed
  first.
- Two kinds of thing sit in the queue: **harness permission requests** (ACP, short-lived,
  denied by default if unanswered) and **review approvals** (submitted artifacts, live
  until decided or stale). They share a queue and differ in card.
- The **phone column** is short. Every "yes" there is a commitment to make that job work
  in one hand on a 390px screen.
- The tray is the queue plus a kill switch. It does not stream output.
- The brokered tools row is there to record that consulta has no human surface at all.

## 3. Content and states

The nouns come from the schema. The states are where the interface work is. Each state
has a one-line answer to "what does the operator see."

### Node

| State | What the operator sees |
|---|---|
| Ready | Plain chip with node name. The default, unmarked. |
| Refused (boundary check failed) | Chip marked critical with the failed check; no sessions offered on this node; still relays and serves. |
| Reachable | Nothing; presence is the default. |
| Unreachable | Chip dims; its sessions show last-seen; its approvals stay in the queue but cannot be decided. |
| Harness version mismatch | Warning on the chip; new sessions on this node blocked with the version pair shown. |

### Hub

| State | What the operator sees |
|---|---|
| Reachable | Nothing. |
| Unreachable | One persistent banner: "Hub unreachable. Local sessions continue; approvals will be delivered when it returns; search is text-only." Not dismissable, not modal. |
| Restored | Banner flips to "Hub reconnected, N items delivered" briefly, then goes. |

### Session

| State | What the operator sees |
|---|---|
| Starting | Row with worktree path and model; no output yet. |
| Running | Streaming output; prompt input enabled; budget meter moving. |
| Waiting on you | Row rises to the top of the queue with the request attached; the session screen shows the request inline above the input. |
| Waiting on a check | "Running `just test`" with elapsed time; prompt input disabled with that reason. |
| Over budget, killed | Row marked killed with the number; output frozen; "resume with more budget" is a new session, not a button on this one. |
| Ended at item close | Row marked closed with the work item; output frozen. |
| Failed | Row marked failed with the last harness error; output frozen. |

### Approval (review)

| State | What the operator sees |
|---|---|
| New | Card in the queue with summary, diff size, node chip, age. |
| Claimed by you | Card shows "claimed" with the claim time; timer running. |
| Stale | Card shows "changed since submit" with the files that moved; approve is disabled; resubmit is the only path. |
| Over size cap | Card shows the size against the cap; approve disabled; the agent is told to split. |
| Decided: approved | Leaves the queue; the session shows the verdict and the broker's post result. |
| Decided: rejected | Leaves the queue; the session shows the verdict and reason. |
| Decided: edited | Leaves the queue; the session shows the `/revise` submission. |

### Permission request (harness)

| State | What the operator sees |
|---|---|
| New | Card with the tool, its arguments, and the node chip. |
| Answered | Leaves the queue. |
| Expired (harness gave up, denied by default) | Leaves the queue; the session shows "denied: unanswered" at that point in the output. |

### Diff

| State | What the operator sees |
|---|---|
| Read-only | Unified diff, file list, per-file blob hash check. |
| Editable (browser only) | Same viewer with edit enabled; "submit as `/revise`" replaces the verdict control. |
| Conflicts with worktree | Files that changed are marked; editing disabled for those files. |

### Work item (Phase 5)

| State | What the operator sees |
|---|---|
| Ready | In the ready list, sorted by the deterministic topo order. |
| Blocked | Shown with what blocks it; cannot be picked for a session. |
| In session | Linked to the session; cannot be picked again. |
| Closed | Out of the list; reachable from its session. |
| Discovered-from | Shows the parent item and the session that found it. |

### Channel

| State | What the operator sees |
|---|---|
| Under ceiling | Cost figure, plain. |
| Near ceiling (>80%) | Cost figure marked warn. |
| At ceiling | Cost figure marked critical; new sessions on this channel refused with that reason. |
| Not bound to this node | The channel's tools and sessions are not offered here; the reason is shown if asked. |

### Memory promotion (Phase 4)

| State | What the operator sees |
|---|---|
| Proposed | In the nightly batch with kind, scope, source session. |
| Promoted / rejected | Out of the batch. |

### Three states designed as first-class, not as badges

- **Waiting on you.** The whole point. One visual treatment, used on the queue card, the
  session row, and the notification.
- **Refused (boundary check failed).** Honest refusal. One treatment on the node chip,
  and the reason wherever the node would otherwise offer work.
- **Degraded (hub unreachable).** Local-first made visible. One banner, one disabled
  style with a reason.

## 4. Flows

Three flows, as text. Each ends with the questions it raised and the decisions taken.

### Approval

```
notification (pager / tray / browser)
  → queue, item at top, state: new
  → open item
      → claim: automatic on open, released on decide, back, or client disconnect
      → state: claimed, timer starts
  → read summary and requirements
  → read diff
      stale?        approve disabled, "resubmit" is the only path
      over cap?     approve disabled, agent told to split
  → decide
      approve   → broker posts the approved bytes → session shows result
      reject    → reason (required, one line) → session shows verdict + reason
      edit      → browser only: editable diff → submit as /revise → session continues
  → release, timer stops
  → next item in the queue, or the queue itself if empty

queue order: waiting-on-you; permission requests before reviews; then oldest first
```

Decisions taken in drawing this:

- **Claim on open, not on a separate control.** Single operator, so claim is a metric,
  not a lock; an explicit "claim" button is friction that measures nothing extra.
  Release happens on decide, on navigating away, or 60 seconds after client
  disconnect, so a dropped socket does not zero the attention count.
- **Reject requires a reason.** The verdict goes back to the agent; a bare reject
  teaches it nothing. One line, required.
- **Stale and over-cap block approve.** Not a warning. The agent resubmits.
- **Edit is browser-only and replaces the verdict.** An edited diff is a `/revise`
  submission, not an approval of something the operator changed.

### Start a session

```
new session
  → choose channel (bound to this node, under ceiling)
  → choose work item                     Phase 1: optional; Phase 5: required, from ready list
  → choose model                          required; no default; validation failure without it
  → set budget                            default from channel policy, editable
  → node chosen by binding                shown, not chosen
  → worktree created from origin/<default>
  → Phase 5: execute gated on a plan artifact from a plan session
  → session screen, state: starting
```

Decisions taken:

- **Model has no default.** The architecture says a spec without a model is a
  validation failure; the form makes it a required field with no preselection, so the
  failure is impossible to reach rather than reported.
- **Node is shown, not picked.** Bindings decide it. If the bound node has refused
  (boundary check failed), the form says which check and cannot start.
- **Work item optional in Phase 1.** The column is nullable from the start; the form
  grows a required picker in Phase 5.

### Degraded

```
hub becomes unreachable
  → banner appears on every surface
  → local sessions continue unchanged
  → auto-allowed actions continue
  → approvals raised locally queue locally; the phone cannot see them
  → recall falls back to FTS; the agent is not told anything changed
  → controls needing the hub disable with the reason

hub returns
  → queued items delivered; banner shows the count briefly
  → phone catches up
```

Decisions taken:

- **The phone is blind during a hub outage.** The phone reads over the hub and has no
  replica; this is already decided. The interface says so on the phone rather than
  showing an empty queue as if nothing is waiting.
- **The agent is not told about degradation.** FTS-only recall is silent. The operator
  sees it in the banner; the harness does not need to.

## Decisions from the flows

Taken 2026-08-24. The first is also recorded in `ARCHITECTURE.md` under the client
crash invariant.

1. **Drafts.** The node holds unsent prompt drafts per session, so a client crash or a
   phone eviction loses nothing typed. Diff edits are desktop-only and live in the
   browser's local storage until submitted as `/revise`; losing one costs a re-edit on a
   machine that was not going to be evicted.
2. **Claim grace period.** A claim releases 60 seconds after the client disconnects. A
   dropped socket does not zero the attention count; a closed laptop does.
3. **Node modes.** There is one. A node enforces or refuses to run harnesses. No
   advisory answers exist anywhere in the interface.
4. **Queue order.** Waiting-on-you first. Within that, harness permission requests
   before review approvals, because permission requests expire and reviews do not.
   Then by age, oldest first.
5. **Kill confirmation.** Confirm on phone and tray, where a stray tap is likely.
   Immediate in the browser.
