# Commands

Every `tracon` command. `--help` on any of them says more.

| | |
|---|---|
| `tracon serve [--listen]` | run the node |
| `tracon setup`, `check-boundary [--deep]` | the boundary: the base image and harness bundle, and an older release's gateway removed |
| `tracon service install\|uninstall\|status\|restart` | the platform supervisor (the desktop app runs these for you) |
| `tracon auth issue [--public-url] [--yes]\|revoke [--yes]\|sessions` | off-machine access; `--public-url` prints the login QR; issue and revoke say which node they change and ask first |
| `tracon external show\|lane [--wait-budget]\|stop <channel>\|start <channel>` | a harness you run yourself, using this node's tools: how to attach it, the headers that label its calls (for Claude Code's `headersHelper`), and refusing or allowing external harnesses on a channel (`clear` is an alias of `start`) |
| `tracon push ls\|rm <id>\|test` | the phones this node pushes to |
| `tracon mesh id\|init\|invite\|members\|remove\|admit`, `enroll` | the mesh |
| `tracon channel create\|list\|bind\|share\|leave` | channels and their bindings |
| `tracon credential import\|ls\|rm\|share\|revokes` | what the broker holds (import is also on Settings); `revokes` lists which peers dropped a removed credential's copy and which are still owed a revoke |
| `tracon doc import\|ls\|get\|put [--format html]\|rm\|archive\|unarchive\|export\|reindex` | documents (import and export are Markdown; `put` writes either format, converting a document in place; `archive` and `unarchive` change only the flag), and rebuilding the vector index from them; import, preview, replace, download, archive, and delete HTML bundles in Documents |
| `tracon session show <package> [--jsonl]` | read a session package offline, with no node running |
| `tracon session archive-legacy\|reopen --harness [--model]` | put sessions from a retired harness away read-only, and carry one forward onto a supported one, on its own model unless `--model` names another |
| `tracon session continue <id>` | carry on a session a node restart cut off, or whose harness died and was not started again, on its workspace and harness (also the session's **Continue** button) |
| `tracon session fork <id> [--to] [--branch] [--note] [--model] [--budget] [--task\|--new-task]` | fork a session: a new one from its branch and context, here or on another node of its channel, while it keeps running (also the session's **Move or fork** panel; see [More nodes](../guides/mesh.md#more-nodes)) |
| `tracon skill import <dir[#git-rev]>\|ls\|rm` | skill packages in a channel's launch manifest |
| `tracon memory ls\|add\|rm\|recall\|batch` | memories, and the promotion batch on demand |
| `tracon work add\|ls\|ready\|show\|edit\|close\|dep\|rm` | the ledger; `--project` and `--priority` take names (`[work]` names the priorities) |
| `tracon work start <id> [--phase] [--node] [--branch] [--base] [--model] [--harness] [--budget] [--repo] [--json]` | start a plan or execute session on an item, here or on a peer, as the Composer does (see [Starting work](../guides/sessions.md#starting-work)) |
| `tracon plugin add <owner/repo\|archive> [--channel]\|ls\|update [name]\|rollback <name>\|rm <name> [--purge]\|logs <name> --channel [-n]\|restart <name> --channel` | plugins: fetched and staged, then installed once you allow the install card; `logs` shows an instance's last lines and `restart` brings an unhealthy one back |
| `tracon repo ls\|show\|set\|rm\|build [repo]` | the `[[repo]]` entries (also **Settings → Repositories**), and building a repository's image from its default-branch Dockerfile |
| `tracon policy keygen\|init\|sign\|push\|show` | the policy bundle |
| `tracon name set\|rm\|show` | a record's name (a session, review, report, approval, manifest, transfer or check), shown by every node in the mesh; `show` says what an id, prefix or name refers to |
| `tracon metrics [--channel] [--days]`, `provenance <sha>` | what happened |
| `tracon gc [--apply] [--caches] [--legacy] [--adopt-unlabelled] [--all]` | runtime storage and images whose owner is over, and when the `[gc]` schedule last ran |
| `tracon sandbox probe` | make one sandbox with a provider outside any session, connect to it, run a command, read its isolation back from the provider, and remove it |

Most commands talk to the running node over its API. The node is `TRACON_URL` when it
is set; else `http://` and `TRACON_LISTEN`, the address `serve` binds; else the address
the node serving this state directory recorded in `<state dir>/serve.json` (ignored, with
a warning, when the process it names is gone); else `http://127.0.0.1:7420`. Before
anything is changed, the node must be the one whose identity is in the state directory;
one that is not is refused, with its name. The check is skipped only for a node named by
`TRACON_URL` with `TRACON_STATE_DIR` left unset, which is how another node (through a
port-forward, say) is driven from a machine that runs its own; with `TRACON_STATE_DIR`
set and no identity in it, only an explicit `TRACON_URL` is written to. `TRACON_URL` and
`TRACON_TOKEN` point the commands at a remote node. The ones that act on this machine
itself ignore both: `setup`, `check-boundary` (which asks this machine's node only when
that node holds the Kubernetes sandbox port), `service`, `enroll`, `mesh id`,
`credential import|ls|rm`, `policy keygen|init|sign|show`, and `session show`,
which reads a file and needs no node at all. `--help` on any of them says more.
