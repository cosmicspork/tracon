# Using the tools from your own harness

Everything above is about harnesses the node starts. A harness you start
yourself — a terminal on this machine — can use the same tools, without ever
holding a credential. Turn it on, then register the node once per channel:

```toml
[external]
enabled = true
```

```sh
tracon external show     # prints the lines below, for each channel
claude mcp add-json --scope user tracon-work \
  '{"type":"http","url":"http://127.0.0.1:7420/mcp/external/work","timeout":1200000,"headersHelper":"tracon external lane --wait-budget 1140"}'
```

The `headersHelper` labels every call with the repository and branch the agent
was started in (`tracon:feat/lanes`), and with the agent's process. The home's
**Your own agents** groups calls by that label, one row per lane, with the last
call, how many there were, what is still waiting on you, and whether the agent's
process is still running. Each row expands into its recent calls; a lane drops
off after a day without a call unless something of it is still open, and
**dismiss** hides one until it calls again. The label is for display only:
nothing is authorized or routed by it. A plain `claude mcp add --transport http
… /mcp/external/work` works too; its calls show as one unlabelled lane. Plugins
calling the node, and a plugin's page calling on your behalf, are no agents of yours:
they stay in the channel's log, and the approval and review cards name them, but they
are never listed here.

The `timeout` lifts Claude Code's per-call limit to twenty minutes, and
`--wait-budget` (or `TRACON_WAIT_BUDGET`) tells the node so: a status tool such
as `review_status` then holds up to ten minutes a call instead of 45 seconds.
Without it the node assumes the 60-second default most MCP clients inherit.
Claude Code before 2.1.274 cut HTTP MCP calls off at about five minutes
whatever `timeout` said; on an older release, drop `--wait-budget` or set it to
300 or less.

There is no session behind an external agent. Every call stands alone, so
connecting, resuming or restarting the agent changes nothing on the node, and
nothing times out.

It gets the channel's tools: the database, the forge and the tracker, the
node's documents and memory, the ledger, and review. `submit_review` takes the
path of your worktree; its repository must live under `[external] repo_roots`
(`~/src` unless you say otherwise), so a linked worktree in a scratch directory
is fine. The node captures the diff from it, skips the container checks (you
ran them where your toolchain is) unless `[external] check_submissions` asks it to run them
behind the submission for the reviewer (the card says they are running until they
land, a failed one shows there without refusing the submission, and a reviewer, whether the trigger starts it or you do, waits for them), and on approval pushes that branch and opens
the change — or updates the one named by `change` — with the brokered
credential, exactly as for a session it started.

The agent sees those checks itself, not only through a reviewer or your notes:
every `review_status` answer to it carries `checks` for the review's latest
commit, with its `head_sha`, a `state` (`running`, `passed`, `failed`,
`could_not_run`, or `none` with a `why`, as when `check_submissions` is off) and
each check's `command`, `outcome`, `exit` and `ms`. Checks settling end a
`review_status` wait early, as a verdict does. The end of a failed check's output
(`tail`) is sent once per commit; later polls carry outcomes only, unless the
agent passes `include_findings: true`. A failure puts a line at the start of the
message naming the check and saying the submission still stands, so the agent
can fix it and resubmit with the same `review_id` without waiting on anyone. A
resubmission on a new commit never shows the last commit's results. So an agent
can run targeted tests itself and leave the full suite to the node.

A decided review has no session to lead back to it, so the history lists it:
**all reviews** beside *Landed recently* on the home (`/reviews`) shows every
decided review this node holds or mirrors, newest first, with its verdict, where
it came from and the change it published; **external only** narrows it to yours.

With `check_submissions` off, a submission runs nothing, but a reviewer still
gets the checks: one started on it (by you, or by an `auto` trigger the channel
sets) first runs the repository's required checks on the review's candidate, as
a submission would have, and starts once they are done. The card says they are
running meanwhile. A repository with no `[[repo]]` entry has none to run: the
reviewer starts on the diff alone, and the card says why no checks ran.

`[external] check_run` gives the agent **`check_run`**, so it tests through the node
instead of a container runtime of its own (the Dev Container CLI, and the Podman
machine it keeps running on macOS). `check_run(worktree, checks?, command?)`
snapshots the worktree's HEAD commit as `submit_review` does (uncommitted
changes are not in it), prepares it as the repository's `[[repo]]` entry says,
and runs the configured checks (or the ones `checks` names, or the one
`command`) in the repository's image on a fresh copy of the prepared tree, with
the dependency cache read-only and no network. They wait for room and are placed
like a review's checks, DigitalOcean included where placement allows. The
outcome, exit status and end of each one's output go back to the agent and are
attached to nothing: no review, no candidate, nothing a later submission reuses.
A call blocks only as long as a status call may (45 s, or longer when the harness
declares its client's timeout with `X-Tracon-Wait-Budget`), so checks that take
longer than its MCP client waits still finish: past that wait it returns
`running` and a `run_id`, the checks keep running, and
`check_run_status(run_id, wait_secs?)` waits for them and returns the same answer
`check_run` would have. `check_run_cancel(run_id)` stops them. A run the agent
stops asking after for 10 minutes is withdrawn, a finished run's answer is kept
for 30 minutes, and a node restart forgets every run; `check_run_status` on such
an id says it is unknown, and the agent calls `check_run` again.
A repository with no `[[repo]]` entry is refused; draft one with
`repo_setup_draft` from a session on it, or add it to `node.toml`. Without
`check_run` the tool is not offered. It changes nothing about submissions:
`check_run` alone leaves them unchecked and the external review trigger at
`manual`. `run_containers`, the one switch 0.60 had for both, is still read for
one release as setting both, with a warning at startup.
`work_close` takes the item's id, and refuses one a running session holds.
`work_list` and `work_show` read the channel's ledger (`worktree` narrows it to
that repository's project), and `work_update` and `work_delete` change or remove
an item once you allow them; neither touches an item a running session holds,
beyond rewording it. A
verb the policy does not name returns at once with an `approval_id` and a card
on your home; nothing has run. Allow it and the node runs the call itself;
`approval_status` tells the agent what became of it. You can edit the
arguments before allowing: the edit is checked against the tool's input schema
first, and one that does not fit, or that changes what the call acts on (`key`,
`project`, `repo`, `number` and `iid` for every tool, plus the tool's own, such
as `doc_write`'s `slug`, `if_hash` and `format` or `retain`'s `kind`), is refused field by
field while the card keeps waiting. Reject with a reason, or request changes with notes, and
`approval_status` returns `rejected` with that reason or `changes_requested`
with the notes. Neither runs anything; the agent revises and calls the tool
again, which asks anew. `GET /api/approvals/{id}` returns one card with the
tool's schema, which arguments are prose and how it is written (`markdown`, or a
format a plugin owns, such as jira-review's `jira_wiki`), and which are locked, and claims it while it is open; `POST /api/approvals/{id}/release` lets it go.

For an investigation or handoff with no code to publish, use **`submit_report`**.
It puts a narrative report in the operator queue and requests a notification,
without a repository, candidate, push, or forge change. The operator can acknowledge
it or request changes; **`report_status`** returns that feedback. Resubmit the same
report ID to revise it. Decisions are tied to the inspected content, so stale
acknowledgements cannot approve a newer report. Acknowledgement is a human action,
independent of whether a push service accepted its notification.

A defect in tracon itself goes to **`report_issue`** instead, and
**`issue_report_status`** says whether the operator published it, as which
GitHub issue, or discarded it.

What it does not get is the boundary. That harness runs as you, on your
machine: the promise here is that it never needs the credential and that every
call it makes is decided by policy and logged, not that it
could not have read the credential by other means. Every call is written to the
channel's external log under its lane. `docs/reference/external-harness-notes.md`
says what that changes; a node holding production credentials should leave
`[external]` off.
