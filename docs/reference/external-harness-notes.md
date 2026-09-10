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
> make is decided by policy and written to a session log.

That is worth having. It removes the reason to go looking, it puts a card in
front of the operator for anything the bundle does not name, and it leaves a
record of what was asked for. It is not the boundary, and the interface, the
README and the config comment all say so rather than letting the word "brokered"
imply more than it means.

Two consequences follow, and neither is mitigated in code:

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

## Shapes that were not obvious

**A session was mandatory, and it was the interesting part.** `Verdict::Ask`
reaches the operator through `Manager::ask_permission`, which needs a live
command channel in `Manager.live`; the queue is `permission_request` rows, whose
`session_id` is a foreign key onto `session`. There is no operator queue
independent of a session. Adding one would have meant relaxing the key and
touching every consumer, so an attachment gets a real session row and a loop
that answers on it, and the whole queue, notification, answer and expiry path
works with no change at all.

**A `Supervisor` was the wrong thing to reuse.** It owns a harness handle, a
runner, a container, a budget and a turn model, and stubbing all five to get a
permission loop is more code than the loop. What it did make sense to share is
the row: `permission_row` and `on_answer_row` are the supervisor's own, lifted
out so both callers ask in exactly the same shape.

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
operator's toolchain is where the operator runs them. Ownership is by channel
rather than by session, since an attachment that idles out comes back as a new
session. `work_close` takes an id, and refuses an item a running session holds,
since closing it would end that session. What stays absent is `review_verdict`,
which belongs to a review session the node starts.

## The policy change that came with it

Comments stopped being allowed unattended in the same bundle (version 5). The
old rule reasoned that a comment moves nothing, which is true of the ticket and
not of the person reading it: the agent speaks in the operator's name where
colleagues see it. With edits and creations arriving as tools at the same time,
the line "writes to the tracker are asked" is easier to hold and easier to
explain than a split between kinds of write. Reads stay free.

## Left for later

- Whether an attachment should expose `Mcp-Session-Id`. The in-container client
  proves POST-per-message works; a client that insists on the header would need
  one echoed per attachment.
- Metrics count an attachment as a session with zero tokens, so a channel's
  session count includes them. Nothing reads that number for a decision yet.
- A peer's interface renders the row with the wording it has; an older peer
  shows a blank branch and model until it updates.
