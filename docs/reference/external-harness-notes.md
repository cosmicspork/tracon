# The external harness, and what it costs

Written during the work, alongside the `[external]` mode. What the roadmap does
not have room for: the guarantee this mode gives up, why it was worth giving
up, and the shapes that were not obvious until they were built.

## The gap it closes

The tools were only ever reachable from inside the boundary. A harness the
operator runs themselves — the ordinary case of sitting down at a terminal —
had no path to them, so the operator's own work went back to the CLIs, which
means back to a credential in a file the agent can read. The node held a better
answer and could not offer it to the case that comes up most.

## What changes when the harness runs as the operator

`ARCHITECTURE.md` says it plainly for the boundaried case: if action
credentials sit on the same UID as the harness, the agent has Bash and can read
them. Outside the boundary that is the situation, and no amount of care in this
mode changes it.

So the claim shrinks, deliberately, from

> the harness cannot read the credential and cannot reach the operator API

to

> the harness never needs the credential, and every call it asks the node to
> make is decided by policy and logged on its channel.

That is worth having. It removes the reason to go looking, it puts a card in
front of the operator for anything the bundle does not name, and it leaves a
record of what was asked for. It is not the boundary, and the interface, the
README and the config comment all say so rather than letting the word "brokered"
imply more than it means.

Three consequences follow, and none is mitigated in code:

- **Loopback self-approval.** The door is the operator API, and so is the
  answer: a process on this machine that can call `POST /mcp/external/{channel}`
  can also call `POST /api/permissions/{id}/answer`. An agent with Bash could
  approve its own card. The door and the answer share one trust, and splitting
  them would mean a second credential the operator has to hold for their own
  machine. A future `[external] answers_need_token` could tighten this; it was
  not built, because a node where that matters should not have `[external]` on
  at all.
- **Credential reach follows the channel.** A credential bound to a channel you
  never attach to stays out of reach, and that is the whole of the containment
  here. Keep production credentials on a channel this mode does not touch.
- **The node itself is within reach.** Its service, its unit and `node.toml`
  belong to the operator, so a harness running as the operator can restart, stop
  or reconfigure the node. That a session cannot restart the node gating it holds
  inside the boundary, where only the CLI and the desktop app reach
  `service.rs`.

## Shapes that were not obvious

**A session was mandatory, and it was the interesting part.** It no longer
carries the asking. `Verdict::Ask` used to reach the operator through
`Manager::ask_permission`, which needed a live command channel and a
`permission_request` row keyed onto the session, and the call blocked until
the operator answered. A client that gave up waiting left a card that ran
nothing when allowed (#272). A brokered call the operator decides is now an
`approval` row the channel owns: the call returns `awaiting_operator` with its
`approval_id` at once, the operator's queue shows it beside permission
requests, allowing it makes the node run the call (with any edit, held to the
same refusals) and keep the result, and the caller reads that result with
`approval_status`, for one id or a list. Every harness asks this way, the
boundary harness included; only a harness's own permission prompt (OpenCode
asking to run a command) still waits, because the harness itself does.

A `report_issue` draft had the same gap: nothing took its `issue_id` back, so
an agent learned its draft was published only by searching the repository's
issues (#362). `issue_report_status` closes it on the same wait contract,
returning the published number, URL and title, or the operator's discard
reason.

The next four shapes describe the attached session external harnesses had
until they stopped getting one; see "No session at all" below.

**A `Supervisor` was the wrong thing to reuse.** It owns a harness handle, a
runner, a container, a budget and a turn model, and stubbing all five to get a
permission loop is more code than the loop. The attachment's loop now handles only a
pause, a resume and the end.

**The row's honest placeholders.** `repo_path` is empty because no repository
was involved, and that immediately turned up in `recent_repos`, which groups
every session by path and would have offered an empty entry in the picker. It
filters on `harness_id` now. `budget_tokens` is recorded and never enforced:
this node brokers no model call for an external harness, so there is nothing to
count, and the interface hides the number rather than showing a full budget
that will never move.

**The operator door, not the harness one.** The harness listener carries no
auth middleware, on the argument that everything able to reach it is already
inside the boundary. A channel-addressed door there would have handed every
boundaried session the run of every channel. On the operator router,
`auth::guard` already answers exactly the question this mode asks: loopback is
the operator, anything else presents the token.

**Review was absent by surface; now it names its worktree.** The first cut left
`submit_review` and `work_close` out, on the reasoning that review needs a
worktree the node made and closing ends the session holding the item. The first
was narrower than it read: the gate was only that an attachment's row carries no
worktree. So the harness names one, and the node accepts it when the
repository (the worktree's common git directory, not the worktree's own path)
is under `[external] repo_roots`; the target records it, and staleness, the
diff editor, and publishing read it from there. Checks are skipped, because the
operator's toolchain is where the operator runs them. The `[review]` cap on lines and files
applies all the same, before anything else runs. Ownership is by channel
rather than by session, since an attachment that idles out comes back as a new
session. `work_close` takes an id, and refuses an item a running session holds,
since closing it would end that session. What stays absent is `review_verdict`,
which belongs to a review session the node starts.

**Review could only open a change; now it can update one.** An external
harness most often works on a branch that already has a pull or merge request
(review feedback, a rebase), and publication's `gh pr create` failed there after
the operator had approved. `submit_review` now takes `change` (the number, fixed
at first submit) and `forge` (a description, a comment, or neither), separate
from `title`/`body`, which are the operator's summary. Submit reads the change
through the brokered CLI, refuses one that is closed, from a fork, or for another
branch or base, and pins the branch head it held as the lease. A worktree that
does not build on that head must say `rewrite: true`; the push is then forced
only over the lease, and never published unattended by a grant. A new submit
for a branch that already has an open change is refused with its number rather
than failing at publication.

**One session per client, not per channel.** The first cut keyed the
attachment on the channel alone, so two terminals on one channel shared a
session: one card queue, one pause, one idle clock, one log. `initialize` now
mints an `Mcp-Session-Id` and returns it as the transport specifies; the key is
(channel, id), and the id is stored as the row's `harness_session_id`. An id the
node no longer holds is not refused with the 404 the spec allows: the client
attaches again under it, because a client that mishandles that 404 would lose
its tools until it restarts, and the node gains nothing by insisting. A client
that echoes no id keeps the old shared attachment. Pause became per client with
it — the paused row is the fence, found again by id after a restart — while
Stop stayed the channel's kill switch, and reviews, reports and `ask_operator`
keys stayed channel-shared so a reattached agent picks up where it left off.

**No session at all.** Per-client sessions keyed on `Mcp-Session-Id` did not
survive contact with the clients. Claude Code 2.1.289 starts a new MCP session on
every resume and restart, re-initializes on a 404 (one 404 made three), never
sends `DELETE`, and opens with a `server/discover` probe carrying no id; omp
holds the id only in memory. Every launch added a row per channel that read
"Attached" for an hour after the process was gone. So the door hands out no id
and reads none, and an external call runs as `Caller::External { lane }` with no
session row (`proposal-external-stateless`).

What the row used to carry moved to where it already belonged:
- The log is the channel's `external_event` table, labelled by lane. Only a
  `tools/call` writes to it, so connecting leaves no trace.
- Reviews, reports, approvals, questions and authority actions record no
  session (the columns became nullable by editing the stored schema, because
  `review` and `candidate` are referenced and a rebuild would have needed
  foreign keys off), and reviews and approvals record the lane.
- Ownership is the channel's external callers. Session-scoped grants never
  match, and `retain` refuses `scope: session` rather than widening it.
- The one fence is the channel's Stop, renamed from Clear to Start for lifting
  it. Per-agent pause went with the session; a channel-wide pause from an older
  node fences like a Stop, and a paused row an older node left becomes one at
  startup.

The lane is what the operator reads instead of a card per session.
`tracon external lane`, run by Claude Code's `headersHelper` in the agent's
working directory, prints `<repository>:<branch>` (the repository named by its
main checkout, so a linked worktree keeps its name) and, in a header of its own,
the agent's pid. The pid is display only: the node notes it with the process's
start time and says a lane is running while one such process is alive, which is
only meaningful because the door is on the operator's own machine. A label is
never authority; any caller can send any label. Two agents in one worktree
share a lane and read as two running processes.

Mesh contract 4 came with it, because a contract 3 peer cannot read a null
`session_id`. A peer does not mirror a session-less approval card: it has no
session to hang the card on, so the card is answered on the node that holds it.

## The policy change that came with it

Comments stopped being allowed unattended in the same bundle (version 5). The
old rule reasoned that a comment moves nothing, which is true of the ticket and
not of the person reading it: the agent speaks in the operator's name where
colleagues see it. With edits and creations arriving as tools at the same time,
the line "writes to the tracker are asked" is easier to hold and easier to
explain than a split between kinds of write. Reads stay free.

## Left for later

- External review decisions and question answers are logged on the channel's
  external log, which the event-based metrics do not read, so an external
  agent's interventions and wait times are missing from them.
- `external_event` grows without a retention policy, and it is not mirrored, so
  a peer's interface does not show another node's lanes.
- A session-less approval card is answered only on the node that holds it.
