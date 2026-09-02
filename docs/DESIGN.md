# Design

Interface design record. Companion to `ARCHITECTURE.md` (the rules) and `ROADMAP.md`
(what is to be built). The screens are the record rather than a separate wireframe
file: `spa/src/app.css` holds the tokens and `spa/src/routes/` the screens. The
visual direction is **Ledger × Tonal**: Instrument Sans with Fragment Mono for every
value, tonal surfaces with no outlines, state carried by a 3px bar and a wash, text
links for actions, dark first with light derived.

## Principles

Each one is kept only because it changes a real decision; the decision it changes is
listed with it.

1. **Waiting-on-you comes first.** The interface exists to direct work, so the home
   screen on every surface is the queue of things blocked on the operator. Running
   output is secondary and never displaces an approval.
   *Decides:* home screen, sort order, what a notification opens to, the first-run
   checklist living in the queue's empty state.

2. **Enforcement is visible.** Every approval shows the node it came from, and a
   node that has refused to run harnesses says so wherever it would otherwise offer
   work. No screen implies a session is gated when the node could not gate it.
   *Decides:* the node chip is part of every card; a refused node shows its failed
   check on the Nodes screen and in the composer.

3. **Degraded is a state, not an error.** Hub unreachable is expected, and the
   interface says what still works. No error toasts for an expected condition.
   *Decides:* a persistent quiet banner instead of modal errors; controls that need
   the hub are disabled with a reason, not hidden; a search that cannot reach its
   embedder says `text only` rather than quietly returning less.

4. **The phone is a full seat; only editing is desktop.** Directing work — starting
   it, adding it, answering for it, provisioning the nodes and providers that run
   it, enrolling a new machine — is read-decide-send and belongs on every surface.
   The one job that genuinely needs a keyboard and a wide screen is editing a diff
   by hand, and the phone is told so in words where the editor would be.
   *Decides:* the diff editor is never rendered on the phone; everything else is
   un-gated, with 16px inputs and stacked layouts under 700px. (This principle
   originally read "the phone directs; it does not edit" and was over-applied to
   work-item creation, provider sign-in, and enrollment — all read-decide-send jobs.
   Reversed in the anywhere batch; the OAuth paste-back in particular belongs on the
   device where the password manager lives.)

5. **The diff is the unit of review.** Not the file tree, not the repo, not the
   transcript. Review means reading a diff against its requirements and returning a
   verdict; editing means editing that diff and submitting it as a request for
   changes.
   *Decides:* no file browser; the diff viewer is the most invested component; the
   requirements sit beside the diff, not the implementation transcript. On the phone
   the file list *is* the diff — each file folds open — because the list decides
   most reviews and 390px will not hold both.

6. **Nothing is lost when the client dies.** Sessions live in the node, so the
   interface never holds state the node does not have. Unsent prompt drafts are held
   by the node per session; diff edits are the one desktop-only exception, in local
   storage keyed by review id and head sha so a resubmission cannot resurrect edits
   against a diff that no longer exists.
   *Decides:* reconnect is silent and resumes where the node is, draft included; the
   login QR's token rides the URL fragment and is stripped before anything renders.

## Jobs and surfaces

Every job, and which surface it works on. "Read" means the surface shows it but
cannot act.

| Job | Browser | Tray | Phone |
|---|---|---|---|
| See what is waiting, across all nodes | yes | yes | yes |
| Answer a permission request | yes | yes | yes |
| Read a diff, return approve / reject | yes | yes | yes |
| Edit a diff and send it back | yes | no | no — told where to |
| Start a session (repo pick, model preselect) | yes | no | yes |
| Add a work item | yes | no | yes |
| Send a prompt into a running session | yes | no | yes |
| Read session output as it streams | yes | no | yes |
| Kill a session | yes | confirm | confirm |
| Connect / disconnect a provider, any node's | yes | no | yes |
| See and share broker credentials | yes | no | yes |
| Enroll a new node (invite + fingerprint confirm) | yes | no | yes |
| Review the nightly memory batch | yes | no | yes |
| Browse work; follow `discovered-from` | yes | no | yes |
| Read and edit documents | yes | no | read |
| See cost per channel against the ceiling | yes | yes | yes |

The tray is the queue plus a kill switch, one level down because it is destructive.
It does not stream output.

## States

The states are where the interface work is; each has a one-line answer to "what does
the operator see."

**Node** — ready: plain chip, unmarked. Refused: critical chip with the failed
check; no sessions offered; still relays and serves. Unreachable: dims, keeps its
place, says when last seen and what it holds that cannot be decided. Version
mismatch: warning with the version pair; new sessions blocked.

**Hub** — reachable: nothing. Unreachable: one persistent, non-dismissable, quiet
banner saying what still works. Restored: "reconnected, N items delivered", briefly.

**Session** — starting, running (streaming, input enabled, meter moving), waiting on
you (rises to the queue top, request inline above the input), waiting on a check
(the command and elapsed time, input disabled with that reason, accent not amber —
it waits on the node, not on you), over budget (killed with the number; resume is a
new session, not a button), ended (labelled by what it produced: planned, reviewed,
item closed), failed (the last harness error, frozen).

**Review** — new, claimed (timer running), stale ("changed since submit" with the
files; approve disabled; resubmit is the only path), over the size cap (approve
disabled; the agent is told to split), decided (leaves the queue; the session shows
the verdict, reason, or publish result).

**Permission** — new (tool, arguments, node chip), answered (leaves the queue),
expired (denied by default; the session shows "denied: unanswered" at that point).

**Provider** — connected (identity, refresh time), pending ("open the link, sign in,
paste what it gives you back" — the paste-back on every surface), failed (reason and
try again), disconnected. A peer's cards render from its hello with the same acts;
the sign-in URL from a peer's ack shows immediately, ahead of the mirror.

**Work item** — ready (in the deterministic order), blocked (named blockers; cannot
be picked), in session (linked), closed (folds), discovered-from (shows the parent
and the session that found it).

**Channel** — under ceiling (plain), near (warn above 80%), at (critical; new
sessions refused with that reason — the 429 shown before the click).

Three states are designed first-class, not as badges: **waiting on you** (one
treatment on card, row, and notification), **refused** (honest refusal, with the
reason wherever the node would otherwise offer work), and **degraded** (one banner,
one disabled-with-reason style).

## Flows

**Approval.** Notification → queue (item at top) → open (claim is automatic on open,
released on decide, back, or 60s after the client vanishes — claim is a metric, not
a lock) → read requirements and diff → decide. Reject requires a one-line reason:
the verdict goes back to the agent, and a bare reject teaches it nothing. Stale and
over-cap block approve rather than warn. An edit replaces the verdict — it *is* a
request for changes.

**Start a session.** Channel (under its ceiling) → repository (recents, managed
clones, browse-a-forge, or a typed path) → work item from the ready list (execute
refused without the item's plan, before the click) → model (required, no silent
default; the last used is preselected when the node offers it) → budget (from the
channel's binding, editable) → node (bindings decide the eligible set; the operator
picks within it; refused, unreachable, and mismatched nodes are listed with the
reason and not selectable).

**First run.** Until the first session exists, the queue's empty state is a
three-step checklist — connect a provider, add a work item, start a plan session —
each linking where it is done, each marked off as the state it derives from
appears. Gone for good once any session exists.

**Enroll.** Invite (channels to hand off, code, QR, the one-line bootstrap, this
node's fingerprint, expiry) → received (the other node's name and fingerprint;
"fingerprints match, admit" or "they differ — cancel") → enrolled. The confirm is
reading two short strings, so the phone does it too.

**Degraded.** Hub gone: banner everywhere, local sessions continue, auto-allowed
actions continue, approvals queue and deliver on return, recall falls back to the
local replica — and the agent is not told anything changed; degradation is the
operator's information, not the model's.

## Decisions

1. **Drafts.** The node holds unsent prompt drafts per session. Diff edits live in
   desktop local storage until submitted; losing one costs a re-edit on the machine
   least likely to be evicted.
2. **Claim grace.** A claim releases 60 seconds after its client disconnects: a
   dropped socket does not zero the attention count; a closed laptop does.
3. **One node mode.** A node enforces or refuses to run harnesses. No advisory
   answers exist anywhere in the interface.
4. **Queue order.** Waiting-on-you first; permission requests before reviews
   (requests expire, reviews do not); then oldest first.
5. **Kill confirmation.** Confirm on phone and tray, where a stray tap is likely;
   immediate in the browser.
6. **Login over plain http off the machine is refused in words** — the Secure
   cookie would silently vanish and loop the login screen, so the screen explains
   the HTTPS requirement instead.
7. **Fixture mode is the screenshot rig.** `TRACON_FIXTURES=1` serves the interface
   against canned state with request-time timestamps, so the README's images
   regenerate without a node and never age.

The build order and what each phase changed live in
[reference/](reference/) and the git history; this file stays the current record.
