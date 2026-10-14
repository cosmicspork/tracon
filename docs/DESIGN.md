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
   by the node per session, and a review's unsent feedback and edited prose per
   review, saved against a version so two devices are told when they disagree rather
   than the last one silently winning; diff edits are the one desktop-only exception,
   in local storage keyed by review id and revision so a resubmission cannot resurrect
   edits against a diff that no longer exists.
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
| Open the session's native harness view | yes, own window | no | yes, in-app frame |
| Kill a session | yes | yes, one submenu down | confirm |
| Connect / disconnect a provider, any node's | yes | no | yes |
| See and share broker credentials | yes | no | yes |
| Enroll a new node (invite + fingerprint confirm) | yes | no | yes |
| Review the nightly memory batch | yes | no | yes |
| Browse work; follow `discovered-from` | yes | no | yes |
| Read and edit documents | yes | no | read |
| See cost per channel against the ceiling | yes | no | yes |
| Administer a node: change any `node.toml` key, a passkey confirming each save away from it | yes | no | yes |

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
clones, browse-a-forge, or a typed path) → optionally a work item from the ready
list (picking one gates its `execute` phase on the item's plan existing, unless
the channel binding waives it; naming none starts a plain, item-free session) →
model (optional — a name here wins as `explicit`; empty falls through the
channel's phase binding, then the channel's own binding, then the node's model
catalogue, and the interface shows which source won; the last used is
preselected when the node offers it) → budget (from the channel's binding,
editable) → node (bindings decide the eligible set; the operator picks within
it; refused, unreachable, and mismatched nodes are listed with the reason and
not selectable).

**First run.** On the desktop the app opens on its own setup page until the node
runs under the user's service manager: podman (and on macOS its machine), the
background service, and the CLI, each with the button or the command that does
it. In the node's interface the composer then carries what stands between it and
a first session — verify the local runtime boundary, connect a provider, and use
a channel. Default local channels satisfy that last prerequisite. Each step links
to its actual settings; a refused boundary is still a prerequisite even when the
provider is connected. Remote access and hub pairing remain a separate optional
Settings link. The checklist returns whenever a prerequisite stops being true.

**Enroll.** Invite (channels to hand off, code, QR, the one-line bootstrap, this
node's fingerprint, expiry) → received (the other node's name and fingerprint;
"fingerprints match, admit" or "they differ — cancel") → enrolled. The confirm is
reading two short strings, so the phone does it too.

**Pair a phone.** `tracon auth issue --url` on the node prints a QR → the phone
opens it, the token in the fragment is spent for a cookie and stripped → the app
offers a passkey straight away ("Add a passkey for this device?"), which this
browser may add without already having one for fifteen minutes, and only while
the node has none → the system passkey sheet → "Passkey added". "Not now" leaves
Settings → Access → Passkeys to do it later, within the same window. Everyday use
asks for nothing more: the cookie is the sign-in.

**Confirm an admin action.** Save (a token, a credential, a grant, a provider key, a
mesh admission, a passkey, any change to the node's configuration) → the node
refuses it with `step_up_required` → a sheet names the action
(`POST /api/authority/grants`), and for a configuration save the keys it changes
("Change node.toml: publish.gh, session.budget_tokens"), and offers Confirm and Cancel →
Confirm opens the passkey sheet → the same save is sent once more with the proof,
and goes through. One sheet per save, however much the save changes. The button is
not decoration: iOS shows the passkey sheet only from a tap. A sheet the system
closed (cancel, timeout, an iOS PWA hang cut off at 60 seconds) says what the
browser reported and offers Try again on the same challenge; Cancel abandons the
save and nothing changes. At the node itself, and in the desktop app on the node's
machine, nothing is refused this way and the sheet never appears. A window that
cannot make passkeys confirms in another browser instead (below); one that can is
offered "Use another device" beside Confirm.

**Administer a node.** Settings → Configuration lists every table of `node.toml`
the bespoke screens do not own (repositories and providers keep theirs), each key
with the control its shape asks for, the key as the file spells it, and whether a
change is live or waits for a restart. A key only the host may change is shown
disabled with the reason; a secret shows whether it is set and offers a field to
replace it, never its value. Edit a table → Save → one request of per-key
operations decided against the version read → the passkey sheet naming the keys
(away from the node) → "Saved", and the restart banner if a restart is now owed. A
version that moved underneath is read again with the edits kept, and a key that
changed on both sides is named with both values for the operator to choose. A
refusal is shown next to the key it is about. Advanced shows the file as the node
reads it, secrets redacted, with its version.

**Sign in from another browser.** The desktop app against a remote node (or any
browser with no token to hand) → the login screen's "Sign in from another
browser" → the window shows a code (`BCDF-GHJK`), an "Open in browser" link and
the same address as a QR, and waits → a browser already signed in to the node
(the system browser, or the phone by scanning) opens it → "Sign in another
window?" with what the asker claims to be, its address and how long ago it
asked, a warning to deny if no window of theirs is asking, and a field for the
code: the operator types the code the window shows (three wrong ones deny it) →
Approve opens the passkey sheet (nothing at the node) → "Approved. Return to the
window that asked." → the window signs in on its own within a few seconds. Deny
ends it on both screens. The phone is told of every sign-in made this way.

**Confirm an admin action from another browser.** The desktop app saves → the
node refuses it with `step_up_required` → the same sheet, which for a window
without passkeys asks the node at once and shows the code, link and QR instead of
Confirm → the approving browser shows "Confirm for another window: Remove a
passkey" with the request beneath and which session asked → type the window's
code, Approve, passkey → the window's sheet closes and the save goes through.
Cancel refuses the request, so the other browser stops offering it. A request
left waiting so long its confirmation expired is sent again for a fresh one,
without a sheet in between.

**Degraded.** Hub gone: banner everywhere, local sessions continue, auto-allowed
actions continue, approvals queue and deliver on return, recall falls back to the
local replica — and the agent is not told anything changed; degradation is the
operator's information, not the model's.

## Decisions

1. **Drafts.** The node holds unsent prompt drafts per session, and each review's
   unsent reason and edited prose until a verdict clears them; the review screen says
   whether its draft is saved, and when another device saved first it shows both and
   asks which to keep. Diff edits live in desktop local storage until submitted;
   losing one costs a re-edit on the machine least likely to be evicted. A prompt
   sent while a turn runs is queued on the node rather than refused, and reaches the
   agent once: appended to the next node tool result the session gets, or as the
   next prompt when the turn ends. The log says which, and when.
2. **Claim grace.** A claim releases 60 seconds after its client disconnects: a
   dropped socket does not zero the attention count; a closed laptop does.
3. **One node mode.** A node enforces or refuses to run harnesses. No advisory
   answers exist anywhere in the interface.
4. **Queue order.** Waiting-on-you first; permission requests before reviews
   (requests expire, reviews do not); then oldest first.
5. **Kill confirmation.** Confirm on the phone, where a stray tap is likely; the
   tray keeps kill one submenu down instead; immediate in the browser.
6. **Login over plain http off the machine is refused in words** — the Secure
   cookie would silently vanish and loop the login screen, so the screen explains
   the HTTPS requirement instead.
7. **Fixture mode is the screenshot rig.** `TRACON_FIXTURES=1` serves the interface
   against canned state with request-time timestamps, so the README's images
   regenerate without a node and never age.
8. **The harness's own UI is an advanced view, never the interface.** OpenCode's web
   app is offered on a session tracon already supervises, on its own origin, in a
   window or a frame the operator opens deliberately. Nothing in tracon's own screens
   depends on it: a node without the bundle offers the control and explains its
   absence, and a Claude Code session is not second-class for having no such view.
9. **Every tool call asks, and no grant is ever saved inside a harness.** An "always"
   clicked in the harness's own permission control is answered "once" and the
   attempted broadening is recorded. A grant the node did not make is one it cannot
   show, revoke, or explain afterwards, so the interface never shows one.
10. **Nothing merges or publishes itself.** The verdict is the operator's, and a
    scoped grant names one target and one commit; a branch that moved needs a fresh
    decision rather than an inherited one. There is no auto-merge to design a state
    for.
11. **The macOS bundle is unsigned, by choice.** The publisher holds no Apple
    Developer ID and will not, so build provenance is what authenticates a download
    and self-update, and Gatekeeper asks once on first open. The install instructions
    say that in words, with the one command that answers it, rather than letting the
    interface imply the warning will not appear.
12. **A repository may carry instructions about itself.** Nested `AGENTS.md` and
    `CLAUDE.md` under a worktree are read by the harness, and that is accepted rather
    than fought: instruction content grants no permission, so such a file can ask for
    something and never approve it. No screen implies those files were filtered out.
13. **The reviewed thing is the tree, not the bytes the agent committed.** The
    candidate, its evidence and the verdict are about a tree. The commits an agent
    made on the way there, and the branch it happened to be on, are prose that ships
    under the operator's name, so they sit beside the diff and are edited like the
    description: by default the node pushes one commit holding exactly the reviewed
    tree, carrying the approved message, on the approved branch. A repository that
    keeps its history pushes the agent's commits, listed for the operator to read
    and each message editable like the squash's. A message or branch that breaks the repository's configured rules is
    refused at submit, so a bad one never reaches the card.
14. **A verdict is never out of reach, and never disabled without a reason.** The
    verdict bar stays on screen however long the review, above the phone's tabs,
    and what answers a verdict (a refusal, a draft not saved, a draft changed on
    another device) shows in it. Request changes and Reject
    each open a labelled composer for the reason the agent will read, instead of
    sitting greyed out until a field elsewhere is filled; an action that cannot be
    taken says why beside it (files changed since submit, a publication to
    reconcile, no token to publish with, a verdict already on its way). A decided
    review says how it was decided, links the change an approval opened, and offers
    no verdict.
15. **A push is a hint, and a refused one says why.** A node with no contact to sign
    pushes with sends none and says so in Settings, instead of sending a push its
    service will refuse. A refusal is shown on the device it was meant for, in the
    service's own words, and never forgets the device: a 403 is usually the node's
    signature, not the phone. What waits on the operator pushes when it starts
    waiting (an approval, a review, a promotion, a question an agent asked); after
    a restart, only what no push service ever took is announced again, once, so a
    redeploy does not re-announce the queue the phone already shows.
16. **A passkey confirms admin, never sign-in.** The cookie a paired browser holds
    is how it signs in, so a lost phone is revoked like any client, and nobody is
    shown a passkey sheet to read their queue. What a passkey guards is what a
    stolen cookie could use to keep its access or reach further: tokens,
    credentials, grants, providers, mesh membership, the node's configuration,
    and the passkeys. Losing every passkey is recovered at the node
    (`docs/RECOVERY.md`), never by a weaker path from afar. A window that cannot
    make one borrows a browser's (device approval) rather than getting a path
    around it: that browser asserts for the exact request, and a session it
    signs in can never enroll the first passkey.
