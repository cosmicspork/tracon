//! The review tools an agent uses to get something published.
//!
//! `submit_review` states an intent; the node captures the diff from the
//! worktree itself, so the artifact under review is what the branch actually
//! contains rather than what the agent says it contains. A session's required
//! checks run in a task the node owns, so a call its client abandons does not
//! take them down with it; `submit_review` waits for them only as long as a
//! status call would. `review_status` waits for the checks and then the
//! verdict, and returns it, including the operator's edits.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    mcp::{
        wait::{wait_secs, MAX_WAIT_SECS},
        CallContext,
    },
    review::{
        self,
        publish::{ChangeRef, Intent, Outputs, Provider, Target},
    },
    session::{state::event_kind as ek, Manager},
    store::{candidate_id, now_ms, CandidateRow, ReviewRevisionRow, ReviewRow, Store},
};

pub const SUBMIT: &str = "submit_review";
pub const STATUS: &str = "review_status";
pub const SUBMIT_REPORT: &str = "submit_report";
pub const REPORT_STATUS: &str = "report_status";
pub const VERDICT: &str = "review_verdict";

pub fn definitions() -> Vec<Value> {
    let wait_secs_arg = json!({
        "type": "integer",
        "description": format!(
            "How long to block, up to {MAX_WAIT_SECS}; larger values are capped at \
             {MAX_WAIT_SECS}. Defaults to {MAX_WAIT_SECS}. 0 returns the current state."
        ),
    });
    vec![
        json!({
            "name": SUBMIT,
            "description": format!(
                "Submit the current branch for human review. The node captures the diff \
                 from the worktree, so commit your work first. In a session the node then runs \
                 the repository's required checks; it waits for them for up to `wait_secs` \
                 ({MAX_WAIT_SECS} at most), and checks that take longer keep running on the \
                 node while this returns `state: \"checking\"` with the review_id to pass to \
                 review_status, which waits for them and then for the verdict. Submitting the \
                 same commit again while its checks run returns the same review_id and starts \
                 nothing. Nothing is published until a human approves; call review_status to \
                 wait for the verdict. Resubmit with the same review_id after making changes, \
                 including after checks failed. A harness you \
                 run yourself passes `worktree`: the absolute path of a worktree whose \
                 repository is under the node's [external] repo_roots; its branch is \
                 what gets pushed. `title` and `body` are your summary for the \
                 operator. Without `change`, approval opens a new pull/merge request \
                 described by `forge.description`, or by title and body when that is \
                 absent. With `change`, approval pushes to that open change's branch \
                 and sends only what `forge` asks for: a replacement description, a \
                 comment, both, or neither."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "worktree": { "type": "string", "description": "Absolute path of the worktree to review. Only for a harness you run yourself." },
                    "title": { "type": "string", "description": "The change's title, as it should appear." },
                    "body": { "type": "string", "description": "What is not obvious from the diff: intent, trade-offs, follow-ups." },
                    "provider": { "type": "string", "enum": ["github", "gitlab"], "description": "In a session on a GitHub or GitLab repository, defaults to that repository's forge; required outside a session." },
                    "project": { "type": "string", "description": "owner/name on GitHub, the project path on GitLab. In a session on a GitHub or GitLab repository, defaults to that repository; required outside a session. Give both or neither when publishing somewhere else." },
                    "base": { "type": "string", "description": "Branch to merge into. Defaults to the branch the worktree was created from, and on a resubmission to the base it was last submitted with. A resubmission may name another base until its change is opened; after that the base is moved on the forge. Resubmitting a commit still waiting for a verdict with another base is such a resubmission, not a retry." },
                    "review_id": { "type": "string", "description": "Set to resubmit an existing review after changes were requested, or after it was published to update the change it opened." },
                    "rerun_checks": { "type": "boolean", "description": "Run configured required checks again even when exact immutable evidence exists. This cannot alter which checks are required." },
                    "wait_secs": {
                        "type": "integer",
                        "description": format!(
                            "How long to wait for the required checks before returning \
                             `checking`, up to {MAX_WAIT_SECS}; larger values are capped. \
                             Defaults to {MAX_WAIT_SECS}. The checks run on either way."
                        ),
                    },
                    "change": { "type": "integer", "description": "The open pull request number or merge request iid this branch already has (pr_for_branch or mr_for_branch finds it). Approval pushes to that change's branch instead of opening a new one. Fixed by the first submit of a review; a review that opened a change updates it without this." },
                    "forge": {
                        "type": "object",
                        "description": "What the forge shows besides the commits. Omit for a new change to describe it with title and body; omit for an existing change to push only.",
                        "properties": {
                            "description": {
                                "type": "object",
                                "description": "Opens a new change with this, or replaces an existing change's title and description.",
                                "properties": {
                                    "title": { "type": "string" },
                                    "body": { "type": "string" },
                                },
                                "required": ["title", "body"],
                            },
                            "comment": { "type": "string", "description": "Posted on the change after the push, e.g. what this revision changed." },
                            "draft": { "type": "boolean", "description": "Open the new change as a draft. Not for an existing change." },
                        },
                    },
                    "message": { "type": "string", "description": "The commit message the change ships with. By default the node pushes one commit holding exactly the reviewed tree, carrying this message (else the description's, else title and body); a repository set to keep its commits pushes yours as written. Either way the operator sees and may edit it." },
                    "branch": { "type": "string", "description": "The forge branch a new change is pushed to, e.g. feat/short-description. Defaults to your worktree's branch, or one named from the title when that is the node's placeholder. Fixed by the first submit of a review. An existing change is pushed to its own branch whatever your worktree's is called." },
                    "rewrite": { "type": "boolean", "description": "The branch's history was rewritten (rebase, amend), so the push replaces what the forge's branch holds: the change's head, or for a branch pushed before its change opened, whatever the branch holds now. Forced only over what it held at submit, and always approved by the operator." },
                },
                "required": ["title", "body", "provider", "project"],
            },
        }),
        json!({
            "name": SUBMIT_REPORT,
            "description": "Submit a standalone narrative report for an operator to acknowledge. \
                            This is not a code review: it needs no repository, commit, diff, \
                            provider, or project, and it can never publish to a forge. Call \
                            report_status to wait for acknowledgement or requested changes. \
                            Resubmit a corrected narrative with the same report_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "A concise report title." },
                    "body": { "type": "string", "description": "The complete narrative report, including observed evidence and recovery context." },
                    "report_id": { "type": "string", "description": "Set after requested changes to replace this report's narrative." },
                },
                "required": ["title", "body"],
            },
        }),
        json!({
            "name": STATUS,
            "description": format!(
                "Wait for a review's verdict and return it. Blocks until the review is \
                 decided or the wait elapses, whichever comes first. The wait is capped \
                 at {MAX_WAIT_SECS} seconds so the call always returns before an MCP \
                 client gives up on it; a human takes far longer than that, so expect to \
                 poll — while the review is undecided this returns `still_waiting` with \
                 the current state, and you call it again. A submission whose required \
                 checks are still running reads `still_waiting` with `state: \"checking\"` \
                 and how long they have run; if they fail it returns `state: \
                 \"submit_failed\"` with the same message submit_review would have \
                 refused with, and nothing was submitted. On approval the node publishes \
                 the approved text itself and returns where it landed."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "review_id": { "type": "string" },
                    "wait_secs": wait_secs_arg.clone(),
                },
                "required": ["review_id"],
            },
        }),
        json!({
            "name": REPORT_STATUS,
            "description": "Wait for a standalone report's operator decision. It never publishes \
                            code. An acknowledgement records receipt only; requested changes \
                            returns notes for a new submit_report with the same report_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "report_id": { "type": "string" },
                    "wait_secs": wait_secs_arg,
                },
                "required": ["report_id"],
            },
        }),
        json!({
            "name": VERDICT,
            "description": "Review sessions only: your verdict on the review this session was \
                            spawned for. It informs the human who decides; it publishes nothing. \
                            The session ends once it is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "verdict": { "type": "string", "enum": ["approve", "request_changes"] },
                    "summary": { "type": "string", "description": "Two or three sentences: what the change does and whether it meets the requirements." },
                    "findings": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "line": { "type": "integer" },
                                "severity": { "type": "string", "enum": ["blocking", "should", "nit"] },
                                "note": { "type": "string" },
                            },
                            "required": ["note"],
                        },
                    },
                },
                "required": ["verdict", "summary"],
            },
        }),
    ]
}

pub async fn call(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        SUBMIT => submit(store, manager, ctx, args).await,
        STATUS => status(store, manager, ctx, args).await,
        SUBMIT_REPORT => submit_report(store, manager, ctx, args).await,
        REPORT_STATUS => report_status(store, ctx, args).await,
        VERDICT => verdict(store, manager, ctx, args).await,
        other => Err(format!("no tool named {other}")),
    }
}

async fn submit(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    // How long this call waits on its checks before handing the wait to
    // `review_status`, counted from the start so capture and the forge's
    // answers come out of the same budget.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_secs(args));
    let session = match ctx.session_id() {
        Some(id) => Some(
            store
                .get_session(id)
                .map_err(|e| e.to_string())?
                .ok_or("this session is gone")?,
        ),
        None => None,
    };
    let external = session.is_none();
    let named = args.get("worktree").and_then(Value::as_str);
    let (worktree, branch, repo_path) = match (external, named) {
        (true, Some(path)) => {
            let roots: Vec<_> = manager
                .cfg()
                .external
                .repo_roots
                .iter()
                .map(|r| crate::config::expand_home(r))
                .collect();
            let at = review::locate_worktree(path, &roots)
                .await
                .map_err(|e| e.to_string())?;
            (
                at.worktree,
                at.branch,
                Some(std::path::PathBuf::from(at.repo)),
            )
        }
        (true, None) => {
            return Err(
                "worktree is required: the absolute path of the worktree to put up for review"
                    .into(),
            )
        }
        (false, Some(_)) => {
            return Err(
                "worktree is for a harness you run yourself; this session reviews its own".into(),
            )
        }
        (false, None) => {
            let session = session
                .as_ref()
                .expect("a caller that is not external has a session");
            (
                manager
                    .snapshot_workspace(&session.id)
                    .await
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
                session.branch.clone(),
                Some(std::path::PathBuf::from(&session.repo_path)),
            )
        }
    };

    let title = str_arg(args, "title")?;
    let body = str_arg(args, "body")?;
    let provider = str_arg(args, "provider")?;
    let project = str_arg(args, "project")?;
    if review::publish::Provider::parse(&provider).is_none() {
        return Err(format!(
            "{provider} is not a provider this node publishes to"
        ));
    }
    // Validate resubmission ownership before capturing or executing anything
    // from this worktree. A review id cannot be used as a way to make another
    // session's candidate consume a check runner.
    let named = args.get("review_id").and_then(Value::as_str);
    // A handle whose checks never became a review: submitting it again opens
    // the review under that id, so the agent keeps the one it was given.
    let mut handle = None;
    let named_review = match named {
        Some(id) => match store.get_review(id).map_err(|error| error.to_string())? {
            Some(existing) => Some(existing),
            None => {
                manager
                    .checking()
                    .get(id)
                    .filter(|entry| Some(entry.session_id.as_str()) == ctx.session_id())
                    .ok_or("no review with that id")?;
                handle = Some(id.to_string());
                None
            }
        },
        None => None,
    };
    let resubmission = match named_review {
        Some(existing) => {
            if !owns(store, ctx, &existing) {
                return Err("that review belongs to another session".into());
            }
            if existing.kind == crate::store::reports::KIND {
                return Err(
                    "that id is a standalone narrative report; revise it with submit_report, never submit_review"
                        .into(),
                );
            }
            let was = serde_json::from_str::<Target>(&existing.target)
                .ok()
                .and_then(|target| target.worktree);
            if external && was.as_deref() != Some(worktree.as_str()) {
                return Err(format!(
                    "resubmit from the worktree the review was made from ({})",
                    was.unwrap_or_default()
                ));
            }
            Some(existing)
        }
        None => None,
    };
    // A client that gave up waiting may retry a submission that is still
    // running or already recorded. Same-commit submits from one worktree run
    // one at a time, and a fresh one finds the checks or the review the other
    // started.
    let head_sha = review::resolve(&worktree, "HEAD")
        .await
        .map_err(|e| e.to_string())?;
    let in_flight = in_flight(&format!(
        "{}\n{}\n{head_sha}",
        ctx.channel,
        ctx.session_id().unwrap_or(&worktree)
    ))
    .await;
    if let Some(session_id) = ctx.session_id() {
        if let Some(entry) = running_checks_of(store, manager, session_id, &ctx.channel, &head_sha)?
        {
            if named.is_none_or(|id| id == entry.handle) {
                return Ok(attached(&entry));
            }
            return Err(format!(
                "checks of this commit are already running under review_id {}; call \
                 review_status with it to wait for them",
                entry.handle
            ));
        }
        if let Some(id) = named {
            if manager
                .checking()
                .get(id)
                .is_some_and(|entry| entry.outcome().is_none())
            {
                return Err(format!(
                    "review {id} is still running checks of an earlier commit; call review_status \
                     to wait for their outcome before submitting it again"
                ));
            }
        }
    }
    let asked_base = args.get("base").and_then(Value::as_str);
    let resubmission = match resubmission {
        Some(existing) => Some(existing),
        None => match pending_duplicate(store, ctx, &worktree, &head_sha)? {
            Some(existing)
                if existing.title == title
                    && existing.body == body
                    && asked_base.is_none_or(|asked| {
                        serde_json::from_str::<Target>(&existing.target)
                            .is_ok_and(|target| target.base == asked)
                    }) =>
            {
                return publish_if_granted(manager, ctx, already_submitted(&existing)).await;
            }
            other => other,
        },
    };
    let stored = match &resubmission {
        Some(existing) => Some(stored_target(store, existing)?),
        None => None,
    };
    // The base defaults to what the worktree was branched from — read from the
    // worktree's `origin/HEAD`, not assumed to be `main` — and for a
    // resubmission to the base it was last submitted with.
    let base = match (asked_base, &stored) {
        (
            Some(asked),
            Some(Target {
                base: was,
                change: Some(change),
                provider,
                ..
            }),
        ) if asked != was => {
            let noun = Provider::parse(provider).map_or("change", |p| p.noun());
            return Err(format!(
                "this review updates {noun} {}, which merges into {was}; an opened change's base \
                 is moved on the forge, not by resubmitting. Resubmit with base {was}, or submit \
                 a new review.",
                change.number
            ));
        }
        (Some(asked), _) => asked.to_string(),
        (None, Some(target)) => target.base.clone(),
        (None, None) => review::default_base(&worktree)
            .await
            .map_err(|e| e.to_string())?,
    };
    // Diff against the remote-tracking ref, so the review shows exactly what the
    // change introduces over what it will merge into.
    let range_base = format!("origin/{base}");
    let capture = review::capture(&worktree, &range_base, &branch)
        .await
        .map_err(|e| e.to_string())?;
    let asked_change = match args.get("change") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or("change is the pull request number or merge request iid")?,
        ),
    };
    let files = serde_json::to_string(&capture.files).unwrap_or_else(|_| "[]".into());
    // Who the commits are by, against the account the review publishes as.
    // Named, never rewritten: re-authoring is the agent's or operator's step.
    let authorship = review::authorship(
        manager.broker(),
        &provider,
        &ctx.channel,
        &ctx.node_id,
        &worktree,
        &range_base,
        &capture.head_sha,
    )
    .await;

    // The cap, before anything else: complexity accretes because nothing says
    // no at submission time. A resubmission is capped the same way.
    let limits = &manager.cfg().review;
    let lines = capture.added + capture.removed;
    if lines > limits.max_diff_lines || capture.files.len() > limits.max_files {
        let reason = format!(
            "the diff is {lines} lines across {} files; the cap is {} lines and {} files. \
             Split the change into smaller submissions.",
            capture.files.len(),
            limits.max_diff_lines,
            limits.max_files
        );
        manager.record_for(
            ctx,
            ek::REVIEW_REJECTED,
            None,
            json!({ "reason": reason, "lines": lines, "files": capture.files.len() }),
        );
        return Err(review::ReviewError::Rejected(reason).to_string());
    }
    // Where this review publishes. A resubmission keeps the target it was
    // first submitted with, including the change it updates; only the base
    // of a change not yet opened may move.
    let mut target = match stored {
        Some(target) => {
            if asked_change.is_some() && asked_change != target.change.as_ref().map(|c| c.number) {
                return Err(
                    "a review updates the change it was first submitted for; submit a new review \
                     to target another"
                        .into(),
                );
            }
            Target {
                base: base.clone(),
                ..target
            }
        }
        None => Target {
            provider: provider.clone(),
            project: project.clone(),
            base: base.clone(),
            branch: branch.clone(),
            worktree: external.then(|| worktree.clone()),
            change: asked_change.map(|number| ChangeRef {
                number,
                url: String::new(),
            }),
        },
    };
    let mut forge = forge_arg(args)?;
    // The branch is the operator's to rename at approval, not the forge
    // output's; the agent proposes it with `branch`.
    forge.branch = None;
    if let Some(message) = args.get("message").and_then(Value::as_str) {
        forge.commit = Some(message.trim().to_string()).filter(|m| !m.is_empty());
    }
    let asked_branch = args
        .get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|b| !b.is_empty());
    if let Some(asked) = asked_branch {
        if asked != target.branch {
            if resubmission.is_some() {
                return Err(format!(
                    "this review pushes to {}; the branch is fixed by its first submit",
                    target.branch
                ));
            }
            if !review::prose::ref_name(asked) {
                return Err(format!("{asked} is not a branch name git accepts"));
            }
            target.branch = asked.to_string();
        }
    } else if resubmission.is_none() && review::prose::placeholder_branch(&target.branch) {
        if let Some(named) = review::prose::branch_from_title(&title) {
            target.branch = named;
        }
    }
    let (commit_mode, style) =
        review::prose::rules(manager.cfg(), store, &ctx.channel, repo_path.as_deref());
    let squash = commit_mode == crate::config::Commits::Squash;
    let commits = review::commits(&worktree, &range_base)
        .await
        .map_err(|e| e.to_string())?;
    // A message or branch that breaks the rules fails here, before the card
    // reaches the operator.
    let mut broken = Vec::new();
    if squash {
        broken.extend(style.check_message(&review::prose::message_for(&forge, &title, &body)));
    } else {
        for commit in &commits {
            broken.extend(
                style
                    .check_message(&commit.subject)
                    .into_iter()
                    .map(|finding| format!("{:.8}: {finding}", commit.sha)),
            );
        }
    }
    if target.change.is_none() {
        broken.extend(style.check_branch(&target.branch));
    }
    if !broken.is_empty() {
        return Err(format!(
            "what would ship breaks this repository's commit rules: {}",
            broken.join("; ")
        ));
    }
    let rewrite = args
        .get("rewrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let last_pushed = match &resubmission {
        Some(existing) if rewrite => store
            .publications_for_review(&existing.id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find_map(|publication| publication.pushed_sha),
        _ => None,
    };
    let mut intent = forge_intent(
        manager,
        ctx,
        &mut target,
        asked_branch.is_some(),
        &capture.head_sha,
        &worktree,
        forge,
        rewrite,
        last_pushed,
        squash.then_some(store.as_ref()),
    )
    .await?;
    intent.commits = commits;
    if squash {
        // Onto what the branch holds when the push adds to it; otherwise
        // where the branch leaves its base, so the change is exactly the
        // reviewed diff. Added to what the branch holds, the squash keeps
        // where the branch leaves its base as a second parent unless the
        // branch already has it: a revision that merged its base to resolve
        // a conflict ships that merge. The lease may be an earlier squash
        // this worktree never had; publication settles that case.
        let leaves = review::merge_base(&worktree, &range_base, &capture.head_sha)
            .await
            .map_err(|e| e.to_string())?;
        match (&intent.lease, intent.rewrite) {
            (Some(lease), false) => {
                intent.squash_onto = Some(lease.clone());
                if !review::descends_from(&worktree, lease, &leaves).await {
                    intent.squash_merges = Some(leaves);
                }
            }
            _ => intent.squash_onto = Some(leaves),
        }
    }
    let intent_json = Some(serde_json::to_string(&intent).map_err(|e| e.to_string())?);
    let max_snapshot_bytes = manager.cfg().supervision.max_snapshot_bytes;
    // Capture the committed candidate before any check can execute. The
    // snapshot is a hardened Git tree import, not this mutable worktree. No
    // check runs here for an external harness, so its submission waits only
    // for the tree hash publication is held to; `retain_candidate_files`
    // keeps its files once the review exists.
    let mut snapshot = if external {
        None
    } else {
        Some(
            review::snapshot_candidate(&worktree, &capture.head_sha, max_snapshot_bytes)
                .await
                .map_err(|error| error.to_string())?,
        )
    };
    let candidate = record_candidate(
        store,
        ctx,
        &worktree,
        &capture.head_sha,
        snapshot.as_mut(),
        max_snapshot_bytes,
    )
    .await?;
    let id = match (&resubmission, handle) {
        (Some(existing), _) => existing.id.clone(),
        (None, Some(handle)) => handle,
        (None, None) => uuid::Uuid::now_v7().to_string(),
    };
    let submission = Submission {
        id,
        session,
        resubmission,
        worktree,
        title,
        body,
        provider,
        target,
        base,
        capture,
        files,
        intent_json,
        candidate,
        max_snapshot_bytes,
        authorship,
    };
    // The operator's own toolchain is where an external harness's checks
    // run, never this node's container: nothing here can execute a command
    // in a worktree the node does not own.
    let Some(session_id) = ctx.session_id() else {
        let checks = review::checks::CheckReport {
            candidate_id: submission.candidate.id.clone(),
            results: Vec::new(),
            required_count: 0,
            all_required_passed: true,
            reused: false,
            // No check ran here, so there is no image to name: whatever the
            // external harness ran is in an environment this node cannot see.
            image: String::new(),
            image_source: "external harness",
            cancelled: None,
            prepared: None,
        };
        let recorded = submission.record(store, manager, ctx, checks).await?;
        return publish_if_granted(manager, ctx, recorded).await;
    };
    let snapshot = snapshot.take().ok_or("checks run on a snapshot")?;
    let commands =
        review::checks::candidate_environment(store, manager.cfg(), &submission.candidate)?.checks;
    let rerun = args
        .get("rerun_checks")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The checks belong to the node from here on, not to this call: a client
    // that gives up on it (Claude Code's HTTP client does at about five
    // minutes) drops this future, and the checks, the review they open and
    // the session's way back out of `waiting_on_check` must not go with it.
    let no_checks = commands.is_empty();
    begin_checks(manager, session_id, &submission.candidate, &commands, rerun);
    let entry = manager.checking().begin(
        &submission.id,
        session_id,
        &submission.candidate.id,
        commands,
    );
    let mut extras = json!({
        "files": submission.capture.files.len(),
        "added": submission.capture.added,
        "removed": submission.capture.removed,
        "uncommitted": submission.capture.uncommitted,
        "authorship": submission.authorship,
    });
    let running = tokio::spawn({
        let store = store.clone();
        let manager = manager.clone();
        let ctx = ctx.clone();
        let session_id = session_id.to_string();
        async move {
            let report = run_checks(
                &store,
                &manager,
                &session_id,
                &submission.candidate,
                &snapshot.root,
                rerun,
            )
            .await;
            tokio::task::spawn_blocking(move || drop(snapshot));
            let report = match report {
                Ok(report) => report,
                Err(refused) => return (Err(refused), false),
            };
            match submission.record(&store, &manager, &ctx, report).await {
                Ok(recorded) => (publish_if_granted(&manager, &ctx, recorded).await, true),
                Err(refused) => (Err(refused), false),
            }
        }
    });
    tokio::spawn({
        let entry = entry.clone();
        let manager = manager.clone();
        let session_id = session_id.to_string();
        async move {
            let (outcome, recorded) = running.await.unwrap_or_else(|error| {
                // Whatever stopped the task, the session is not left
                // waiting on checks nobody is running.
                manager.set_checking(&session_id, false);
                (
                    Err(format!(
                        "the checks stopped without an outcome ({error}). Submit again."
                    )),
                    false,
                )
            });
            entry.finish(outcome, recorded);
        }
    });
    // Nothing else waits on this slot now: a retry finds the running entry.
    drop(in_flight);
    // With nothing required there is nothing slow to wait for, and the
    // review is answered here as it always was.
    let deadline = if no_checks {
        deadline.max(tokio::time::Instant::now() + std::time::Duration::from_secs(MAX_WAIT_SECS))
    } else {
        deadline
    };
    match entry.wait(deadline).await {
        Some(outcome) => outcome,
        None => {
            let mut response = checking(&entry);
            if let (Some(response), Some(extras)) =
                (response.as_object_mut(), extras.as_object_mut())
            {
                response.append(extras);
            }
            Ok(response)
        }
    }
}

/// Everything a submission settled before its checks, which is what records
/// the review once they pass.
struct Submission {
    /// The review this revises, or the id the new one is opened under.
    id: String,
    session: Option<crate::store::SessionRow>,
    resubmission: Option<ReviewRow>,
    worktree: String,
    title: String,
    body: String,
    provider: String,
    target: Target,
    base: String,
    capture: review::Capture,
    files: String,
    intent_json: Option<String>,
    candidate: CandidateRow,
    max_snapshot_bytes: u64,
    authorship: Option<Value>,
}

impl Submission {
    /// Open or revise the review on checks that passed, or on none for an
    /// external harness.
    async fn record(
        self,
        store: &Arc<Store>,
        manager: &Manager,
        ctx: &CallContext,
        checks: review::checks::CheckReport,
    ) -> Result<Value, String> {
        let Submission {
            id,
            session,
            resubmission,
            worktree,
            title,
            body,
            provider,
            target,
            base,
            capture,
            files,
            intent_json,
            candidate,
            max_snapshot_bytes,
            authorship,
        } = self;
        let external = session.is_none();
        let checks_json = (!external)
            .then(|| serde_json::to_string(&checks.results).unwrap_or_else(|_| "[]".into()));

        // Requirements shown on the review screen are what this revision was
        // actually checked against, read once here and pinned to the revision —
        // never the (mutable) work item re-read at display time.
        let requirements = session
            .as_ref()
            .and_then(|s| s.work_item_id.as_ref())
            .and_then(|work_item_id| store.work_get(work_item_id).ok().flatten());
        let requirements_title = requirements.as_ref().map(|w| w.title.clone());
        let requirements_body = requirements.as_ref().map(|w| w.body.clone());
        let requirements_hash = requirements
            .as_ref()
            .map(|w| crate::corpus::hash_body(&format!("{}\n{}", w.title, w.body)));

        // A resubmission keeps the same card and the same thread.
        if resubmission.is_some() {
            let revision = ReviewRevisionRow {
                id: uuid::Uuid::now_v7().to_string(),
                review_id: id.clone(),
                candidate_id: candidate.id.clone(),
                title: title.clone(),
                body: body.clone(),
                diff: capture.diff.clone(),
                files: files.clone(),
                head_sha: capture.head_sha.clone(),
                context_json: serde_json::to_string(&capture.contexts)
                    .unwrap_or_else(|_| "[]".into()),
                requirements_work_item_id: session.as_ref().and_then(|s| s.work_item_id.clone()),
                requirements_title,
                requirements_body,
                requirements_hash,
                created_ms: now_ms(),
                intent_json,
            };
            let revised = store
                .revise_review_with_revision(
                    &id,
                    &title,
                    &body,
                    &serde_json::to_string(&target).map_err(|e| e.to_string())?,
                    &base,
                    capture.added,
                    capture.removed,
                    checks_json.as_deref(),
                    &revision,
                )
                .map_err(|error| error.to_string())?;
            if !revised {
                return Err(format!(
                    "review {id} is currently being published and cannot be resubmitted; call \
                     review_status to wait for the verdict"
                ));
            }
            retain_candidate_files(store, &worktree, &candidate, max_snapshot_bytes);
            manager.publish_queue().await;
            let reviewer = spawn_review_session(store, manager, ctx, &id, session.as_ref()).await;
            return Ok(json!({
                "review_id": id,
                "state": "new",
                "message": "Resubmitted. Call review_status to wait for the verdict.",
                "files": capture.files.len(),
                "added": capture.added,
                "removed": capture.removed,
                "uncommitted": capture.uncommitted,
                "review_session": reviewer,
                "candidate_id": candidate.id,
                "checks_reused": checks.reused,
                "authorship": authorship,
            }));
        }

        let row = ReviewRow {
            id: id.clone(),
            session_id: ctx.session_id().map(str::to_string),
            node_id: ctx.node_id.clone(),
            channel: ctx.channel.clone(),
            kind: if provider == "gitlab" {
                "mr".into()
            } else {
                "pr".into()
            },
            title,
            body,
            edited_title: None,
            edited_body: None,
            provider,
            target: serde_json::to_string(&target).unwrap_or_default(),
            diff: capture.diff,
            files,
            head_sha: capture.head_sha,
            base_ref: base,
            added: capture.added,
            removed: capture.removed,
            state: "new".into(),
            verdict_reason: None,
            publish_result: None,
            claimed_ms: None,
            created_ms: now_ms(),
            created_mono_ms: 0,
            resolved_mono_ms: None,
            updated_ms: now_ms(),
            checks_json,
            review_session_id: None,
            ai_verdict_json: None,
            revision_patch: None,
            lane: ctx.lane().map(str::to_string),
        };
        let revision = ReviewRevisionRow {
            id: uuid::Uuid::now_v7().to_string(),
            review_id: id.clone(),
            candidate_id: candidate.id.clone(),
            title: row.title.clone(),
            body: row.body.clone(),
            diff: row.diff.clone(),
            files: row.files.clone(),
            head_sha: row.head_sha.clone(),
            context_json: serde_json::to_string(&capture.contexts).unwrap_or_else(|_| "[]".into()),
            requirements_work_item_id: session.as_ref().and_then(|s| s.work_item_id.clone()),
            requirements_title,
            requirements_body,
            requirements_hash,
            created_ms: row.created_ms,
            intent_json,
        };
        store
            .insert_review_with_revision(&row, &revision)
            .map_err(|error| error.to_string())?;
        retain_candidate_files(store, &worktree, &candidate, max_snapshot_bytes);
        manager.publish_queue().await;
        let reviewer = spawn_review_session(store, manager, ctx, &id, session.as_ref()).await;

        Ok(json!({
            "review_id": id,
            "state": "new",
            "message": "Submitted for review. Nothing is published until a human approves. \
                        Call review_status to wait for the verdict.",
            "files": capture.files.len(),
            "added": row.added,
            "removed": row.removed,
            "uncommitted": capture.uncommitted,
            "review_session": reviewer,
            "candidate_id": candidate.id,
            "checks_reused": checks.reused,
            "authorship": authorship,
        }))
    }
}

/// A recorded review publishes at once when an authority grant covers it,
/// exactly as if the operator had approved it; otherwise the response says
/// what publication would need.
async fn publish_if_granted(
    manager: &Manager,
    ctx: &CallContext,
    submitted: Value,
) -> Result<Value, String> {
    manager.tools.auto_publish_review(ctx, submitted).await
}

/// Checks this session already has running on `head_sha`, against the check
/// definitions its candidate answers to now.
fn running_checks_of(
    store: &Store,
    manager: &Manager,
    session_id: &str,
    channel: &str,
    head_sha: &str,
) -> Result<Option<Arc<crate::mcp::checking::Entry>>, String> {
    let Some(candidate) = store
        .candidate(&candidate_id(head_sha, channel))
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let commands = review::checks::candidate_environment(store, manager.cfg(), &candidate)?.checks;
    Ok(manager.checking().running(&crate::mcp::checking::key(
        session_id,
        &candidate.id,
        &commands,
    )))
}

/// What a caller is told while checks run: the handle to wait on, and what is
/// running.
fn checking(entry: &crate::mcp::checking::Entry) -> Value {
    json!({
        "review_id": entry.handle,
        "state": "checking",
        "candidate_id": entry.candidate_id,
        "checks": entry.commands,
        "running_secs": entry.running_secs(),
        "message": "The required checks are running on the node; nothing reaches the operator \
                    until they pass. Call review_status with this review_id to wait for them: it \
                    returns the review once they pass, or what failed.",
    })
}

/// A retried submission of a commit whose checks are already running.
fn attached(entry: &crate::mcp::checking::Entry) -> Value {
    let mut response = checking(entry);
    response["attached"] = json!(true);
    response["message"] = json!(
        "Checks of this commit are already running for this submission; nothing new was \
         started, and this call's title, body and other arguments were not used. Call \
         review_status with this review_id to wait for the outcome, then resubmit with it if \
         the words should change."
    );
    response
}

/// Holds the submission slot for `key` until dropped.
struct InFlight {
    key: String,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

type Slots = std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

fn slots() -> &'static Slots {
    static SLOTS: std::sync::OnceLock<Slots> = std::sync::OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

async fn in_flight(key: &str) -> InFlight {
    let slot = slots()
        .lock()
        .unwrap()
        .entry(key.to_string())
        .or_default()
        .clone();
    InFlight {
        key: key.to_string(),
        _guard: slot.lock_owned().await,
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut slots = slots().lock().unwrap();
        // Two references: the map's and this guard's. Anything more is a
        // submission still waiting for the slot.
        if slots
            .get(&self.key)
            .is_some_and(|slot| Arc::strong_count(slot) <= 2)
        {
            slots.remove(&self.key);
        }
    }
}

/// An undecided review this caller already made of `head_sha` from this
/// worktree. A fresh submission with the same title, body and base is
/// answered with it; one with different words or another base revises it,
/// as if it named it.
fn pending_duplicate(
    store: &Store,
    ctx: &CallContext,
    worktree: &str,
    head_sha: &str,
) -> Result<Option<ReviewRow>, String> {
    let external = ctx.session_id().is_none();
    Ok(store
        .undecided_reviews_at(&ctx.channel, &ctx.node_id, head_sha)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|review| {
            owns(store, ctx, review)
                && (!external
                    || serde_json::from_str::<Target>(&review.target)
                        .ok()
                        .and_then(|target| target.worktree)
                        .as_deref()
                        == Some(worktree))
        }))
}

fn already_submitted(existing: &ReviewRow) -> Value {
    let files = serde_json::from_str::<Vec<Value>>(&existing.files)
        .map(|files| files.len())
        .unwrap_or_default();
    json!({
        "review_id": existing.id,
        "state": existing.state,
        "message": "This commit is already submitted with these words and waiting for a verdict. \
                    Call review_status to wait for it.",
        "files": files,
        "added": existing.added,
        "removed": existing.removed,
        "already_submitted": true,
    })
}

/// Record the candidate for `head_sha`, with the files of `snapshot` when
/// there is one. Without one only the tree hash is recorded, and the
/// candidate says it is not yet materialized.
async fn record_candidate(
    store: &Arc<Store>,
    ctx: &CallContext,
    worktree: &str,
    head_sha: &str,
    snapshot: Option<&mut review::CandidateSnapshot>,
    max_snapshot_bytes: u64,
) -> Result<CandidateRow, String> {
    let tree_sha = match &snapshot {
        Some(snapshot) => snapshot.tree_sha.clone(),
        None => review::resolve(worktree, &format!("{head_sha}^{{tree}}"))
            .await
            .map_err(|error| error.to_string())?,
    };
    let row = CandidateRow {
        id: candidate_id(head_sha, &ctx.channel),
        head_sha: head_sha.to_string(),
        tree_sha: Some(tree_sha),
        channel: ctx.channel.clone(),
        owner_session_id: ctx.session_id().map(str::to_string),
        source_kind: "git".into(),
        captured_ms: now_ms(),
        capture_json: json!({
            "method": "hardened_git_tree",
            "materialized": snapshot.is_some(),
            "max_snapshot_bytes": max_snapshot_bytes,
        })
        .to_string(),
    };
    let files = snapshot.map(|snapshot| std::mem::take(&mut snapshot.files));
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let created = store
            .insert_candidate(&row)
            .map_err(|error| error.to_string())?;
        if !created {
            let tree_sha = row.tree_sha.as_deref().unwrap_or_default();
            store
                .record_candidate_snapshot(&row.id, tree_sha, &row.capture_json)
                .map_err(|error| error.to_string())?;
        }
        if let Some(files) = files {
            store
                .insert_candidate_files(&row.id, &files)
                .map_err(|error| error.to_string())?;
            store
                .record_candidate_materialized(&row.id)
                .map_err(|error| error.to_string())?;
        }
        store
            .candidate(&row.id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "candidate disappeared while it was captured".to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Read and keep a candidate's files after its review is recorded, so a
/// transfer can carry them without the submitting client waiting on a
/// whole-tree read and its insert. A candidate that already has them is left
/// alone; one that cannot be read stays unmaterialized, which a transfer
/// refuses.
fn retain_candidate_files(
    store: &Arc<Store>,
    worktree: &str,
    candidate: &CandidateRow,
    max_snapshot_bytes: u64,
) {
    let materialized = serde_json::from_str::<Value>(&candidate.capture_json)
        .ok()
        .and_then(|capture| capture["materialized"].as_bool())
        .unwrap_or(false);
    if materialized {
        return;
    }
    let store = store.clone();
    let worktree = worktree.to_string();
    let id = candidate.id.clone();
    let head_sha = candidate.head_sha.clone();
    tokio::spawn(async move {
        let retained = match review::read_candidate(&worktree, &head_sha, max_snapshot_bytes).await
        {
            Ok(tree) => tokio::task::spawn_blocking(move || {
                store.insert_candidate_files(&id, &tree.files)?;
                store.record_candidate_materialized(&id)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|stored| stored.map_err(|error| error.to_string())),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = retained {
            tracing::warn!(%error, head_sha, "could not keep a candidate's files");
        }
    });
}

fn forge_arg(args: &Value) -> Result<Outputs, String> {
    let forge: Outputs = match args.get("forge") {
        None | Some(Value::Null) => Outputs::default(),
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| format!("forge: {e}"))?,
    };
    if forge
        .description
        .as_ref()
        .is_some_and(|d| d.title.trim().is_empty())
    {
        return Err("forge.description needs a title".into());
    }
    Ok(Outputs {
        comment: forge.comment.filter(|c| !c.trim().is_empty()),
        ..forge
    })
}

/// Where a resubmission publishes: the review's stored target, and for a
/// review published before publication recorded the change it opened, that
/// change and the branch it was pushed to, read from the publication record.
fn stored_target(store: &Store, review: &ReviewRow) -> Result<Target, String> {
    let target = serde_json::from_str::<Target>(&review.target)
        .map_err(|e| format!("this review's target is unreadable: {e}"))?;
    if target.change.is_some() {
        return Ok(target);
    }
    let opened = store
        .publications_for_review(&review.id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|publication| publication.state == "opened");
    Ok(
        match opened.and_then(|publication| Some((publication.result?, publication.branch))) {
            Some((url, branch)) => Target { branch, ..target }.opened(&url),
            None => target,
        },
    )
}

/// Settle what this revision may do on the forge, asking the forge rather
/// than finding out after the operator approved. An existing change must be
/// open and of this repository into this base, and is pushed to on its own
/// branch unless the agent named another (`branch_named`), which must then be
/// the change's; what its branch holds now is the only thing the push may
/// replace. A new change must not
/// collide with one already open for the branch, and a declared rewrite of a
/// branch pushed without one may replace only what the branch holds now
/// (`last_pushed` when the forge cannot say).
#[allow(clippy::too_many_arguments)]
async fn forge_intent(
    manager: &Manager,
    ctx: &CallContext,
    target: &mut Target,
    branch_named: bool,
    head_sha: &str,
    worktree: &str,
    forge: Outputs,
    rewrite: bool,
    last_pushed: Option<String>,
    squashed_by: Option<&Store>,
) -> Result<Intent, String> {
    let provider = Provider::parse(&target.provider).ok_or_else(|| {
        format!(
            "{} is not a provider this node publishes to",
            target.provider
        )
    })?;
    let noun = provider.noun();
    let env =
        manager
            .broker()
            .read()
            .unwrap()
            .env_for(provider.credential(), &ctx.channel, &ctx.node_id);
    let dir = review::publish::inspection_dir().map_err(|e| e.to_string())?;
    let cfg = manager.cfg();
    let Some(change) = target.change.clone() else {
        // Opening a second change for a branch fails at publication, after
        // the operator approved it. Where the forge can be asked, say so now.
        if let Ok(env) = &env {
            match review::publish::open_change_for_branch(provider, cfg, &dir, env, target).await {
                Ok(Some((number, url))) => {
                    return Err(format!(
                        "{noun} {number} ({url}) is already open for {}; pass change: {number} to \
                         update it",
                        target.branch
                    ))
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    %error,
                    "could not ask the forge whether this branch already has an open change"
                ),
            }
        }
        if !rewrite {
            return Ok(Intent {
                forge,
                lease: None,
                rewrite: false,
                ..Intent::default()
            });
        }
        // A branch can be on the forge without a change ever opening (the
        // open failed after the push), and a rebase must replace it.
        let asked = match &env {
            Ok(env) => review::publish::branch_head(provider, cfg, &dir, env, target)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let lease = match (asked, last_pushed) {
            (Ok(held), _) => held,
            (Err(_), Some(pushed)) => Some(pushed),
            (Err(error), None) => {
                return Err(format!(
                    "could not read what {} holds on the forge, so a rewrite has nothing to \
                     lease on: {error}",
                    target.branch
                ))
            }
        };
        let Some(lease) = lease else {
            // Nothing there yet: an ordinary first push replaces nothing.
            return Ok(Intent {
                forge,
                lease: None,
                rewrite: false,
                ..Intent::default()
            });
        };
        let builds_on =
            lease == head_sha || review::descends_from(worktree, head_sha, &lease).await;
        return Ok(Intent {
            forge,
            lease: Some(lease),
            rewrite: !builds_on,
            ..Intent::default()
        });
    };
    let number = change.number;
    if forge.draft {
        return Err(format!("draft is for a new change, not {noun} {number}"));
    }
    let env = env.map_err(|e| {
        format!("updating {noun} {number} needs the forge credential on this channel: {e}")
    })?;
    let state = review::publish::change_state(provider, cfg, &dir, &env, target, number)
        .await
        .map_err(|e| format!("could not read {noun} {number}: {e}"))?;
    // The worktree's branch name is local: the push goes to the change's
    // branch, and the lease below holds it to building on what that holds.
    if !branch_named {
        target.branch = state.source_branch.clone();
    }
    if let Some(refusal) = state.refusal(provider, target, number) {
        return Err(refusal);
    }
    target.change = Some(ChangeRef {
        number,
        url: state.url.clone(),
    });
    let lease = state.head_sha;
    let mut builds_on =
        lease == head_sha || review::descends_from(worktree, head_sha, &lease).await;
    // What the branch holds may be this node's own earlier squash, which
    // the agent never had: building on the commit that squash was made from
    // is building on it.
    if let (false, Some(store)) = (builds_on, squashed_by) {
        let pushed = store
            .publications_that_pushed(&ctx.channel, &target.project, &lease)
            .map_err(|e| e.to_string())?;
        for publication in pushed {
            let from = &publication.head_sha;
            if from == head_sha || review::descends_from(worktree, head_sha, from).await {
                builds_on = true;
                break;
            }
        }
    }
    if !builds_on && !rewrite {
        return Err(format!(
            "{noun} {number}'s branch holds {lease:.8}, which {head_sha:.8} does not build on. \
             Rebase onto it, or pass rewrite: true to replace that history (the operator always \
             approves a rewrite)."
        ));
    }
    Ok(Intent {
        forge,
        lease: Some(lease),
        // A branch that builds on what the change holds is a fast-forward,
        // whatever was asked: nothing is thrown away, so nothing is forced.
        rewrite: rewrite && !builds_on,
        ..Intent::default()
    })
}

/// Submit a standalone report without touching a worktree, Git, a provider, or
/// a candidate. A calling session and its channel are checked again here so it
/// cannot name another session's report scope.
async fn submit_report(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    if let Some(id) = ctx.session_id() {
        let session = store
            .get_session(id)
            .map_err(|e| e.to_string())?
            .ok_or("this session is gone")?;
        if session.channel != ctx.channel || session.node_id != ctx.node_id {
            return Err(
                "this session is not attached to the requested channel on this node".into(),
            );
        }
    }
    let title = str_arg(args, "title")?.trim().to_string();
    let body = str_arg(args, "body")?.trim().to_string();
    review::report::validate(&title, &body)?;
    let content_hash = review::report::content_hash(&title, &body);

    if let Some(id) = args
        .get("report_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
    {
        let report = store
            .get_review(id)
            .map_err(|error| error.to_string())?
            .ok_or("no report with that id")?;
        if report.kind != crate::store::reports::KIND {
            return Err("report_id names a code review; use submit_review for it".into());
        }
        if !owns(store, ctx, &report) || report.node_id != ctx.node_id {
            return Err("that report belongs to another session, channel, or node".into());
        }
        if !store
            .resubmit_report(id, &title, &body, &content_hash)
            .map_err(|error| error.to_string())?
        {
            return Err(
                "this report is no longer awaiting a revision; call report_status before submitting again"
                    .into(),
            );
        }
        manager.publish_queue().await;
        return Ok(json!({
            "report_id": id,
            "state": "new",
            "message": "Report resubmitted for acknowledgement. It has no code or publication path.",
        }));
    }

    let id = uuid::Uuid::now_v7().to_string();
    let now = now_ms();
    let report = ReviewRow {
        id: id.clone(),
        session_id: ctx.session_id().map(str::to_string),
        node_id: ctx.node_id.clone(),
        channel: ctx.channel.clone(),
        kind: crate::store::reports::KIND.into(),
        title,
        body,
        edited_title: None,
        edited_body: None,
        provider: "none".into(),
        target: json!({ "kind": "narrative_report", "session_id": ctx.session_id(), "lane": ctx.lane() })
            .to_string(),
        diff: String::new(),
        files: "[]".into(),
        head_sha: content_hash,
        base_ref: "none".into(),
        added: 0,
        removed: 0,
        state: "new".into(),
        verdict_reason: None,
        publish_result: None,
        claimed_ms: None,
        created_ms: now,
        created_mono_ms: 0,
        resolved_mono_ms: None,
        updated_ms: now,
        checks_json: None,
        review_session_id: None,
        ai_verdict_json: None,
        revision_patch: None,
        lane: ctx.lane().map(str::to_string),
    };
    store
        .insert_review(&report)
        .map_err(|error| error.to_string())?;
    manager.publish_queue().await;
    Ok(json!({
        "report_id": id,
        "state": "new",
        "message": "Submitted for operator acknowledgement. This standalone report has no Git, code-review, or forge-publication path.",
    }))
}
/// Whose review this is. A session owns what it submitted. A harness the
/// operator runs themselves has no session, so what such a harness submitted
/// belongs to the channel's external callers: no session, or one of the
/// sessions those harnesses attached as before calls went session-less.
fn owns(store: &Store, ctx: &CallContext, r: &ReviewRow) -> bool {
    caller_owns(store, ctx, &r.channel, r.session_id.as_deref())
}

/// [`owns`] for any row that records the channel and session it came from.
pub(crate) fn caller_owns(
    store: &Store,
    ctx: &CallContext,
    channel: &str,
    session_id: Option<&str>,
) -> bool {
    if let Some(id) = ctx.session_id() {
        return session_id == Some(id);
    }
    let external = |id: &str| {
        store
            .get_session(id)
            .ok()
            .flatten()
            .is_some_and(|s| s.harness_id == crate::session::external::HARNESS_ID)
    };
    channel == ctx.channel && session_id.is_none_or(external)
}

/// The session that asked for these checks, as the authority on whether they
/// should still be running. A check is minutes of subprocess inside the
/// boundary: the operator can pause or stop the session while one is
/// executing, and when they do, the execution stops rather than running to
/// completion and reporting a verdict nobody is waiting for.
struct SessionStillWants {
    store: Arc<Store>,
    session_id: String,
}

impl review::checks::Cancel for SessionStillWants {
    fn cancelled(&self) -> Option<String> {
        let row = self.store.get_session(&self.session_id).ok().flatten()?;
        let state = crate::session::state::SessionState::from_stored(&row.state);
        // `waiting_on_check` is what a checking session sits in; anything
        // fenced or ended withdraws the request.
        if state == crate::session::state::SessionState::Paused {
            return Some("the session was paused".into());
        }
        if state.is_terminal() {
            return Some(format!("the session ended ({})", row.state));
        }
        None
    }
}

/// Put the session in `waiting_on_check` and say what is starting, before the
/// submitting call can return: by the time an agent hears `checking`, the
/// session and its log already say so.
fn begin_checks(
    manager: &Manager,
    session_id: &str,
    candidate: &CandidateRow,
    commands: &[String],
    force_rerun: bool,
) {
    manager.set_checking(session_id, true);
    manager.record_event(
        session_id,
        ek::CHECK_STARTED,
        json!({
            "candidate_id": candidate.id,
            "head_sha": candidate.head_sha,
            "commands": commands,
            "rerun": force_rerun,
        }),
    );
}

/// Required checks against the immutable candidate snapshot, after
/// [`begin_checks`]. The candidate owner receives the verification milestone
/// even if a later prose-only revision is submitted by another attached
/// session.
async fn run_checks(
    store: &Arc<Store>,
    manager: &Manager,
    session_id: &str,
    candidate: &CandidateRow,
    snapshot: &std::path::Path,
    force_rerun: bool,
) -> Result<review::checks::CheckReport, String> {
    let report = review::checks::run_required(
        manager.backend().as_ref(),
        manager.cfg(),
        store,
        candidate,
        snapshot,
        force_rerun,
        &SessionStillWants {
            store: store.clone(),
            session_id: session_id.to_string(),
        },
    )
    .await;
    manager.set_checking(session_id, false);
    let report = report?;
    if let Some(prepared) = &report.prepared {
        manager.record_event(
            session_id,
            ek::CHECK_PREPARED,
            json!({ "candidate_id": candidate.id, "ok": prepared.ok, "ms": prepared.ms }),
        );
    }
    for result in &report.results {
        manager.record_event(
            session_id,
            ek::CHECK_RESULT,
            json!({
                "candidate_id": candidate.id,
                "command": result.command,
                "ok": result.ok,
                "exit": result.exit,
                "tail": result.tail,
                "failures": result.failures,
                "ms": result.ms,
                "outcome": result.outcome,
                "reused_from": result.reused_from,
            }),
        );
    }
    // A cancelled run is not a failed candidate and not a passed one: the
    // operator withdrew the session while it was executing. Nothing is
    // verified, nothing is rejected, and the submission stops here rather
    // than opening a review on evidence that was never finished.
    if let Some(reason) = &report.cancelled {
        manager.record_event(
            session_id,
            ek::CHECK_CANCELLED,
            json!({
                "candidate_id": candidate.id,
                "head_sha": candidate.head_sha,
                "reason": reason,
            }),
        );
        return Err(format!(
            "checks were cancelled: {reason}. Nothing was verified and no review was opened."
        ));
    }
    if report.all_required_passed {
        if let Some(owner) = candidate.owner_session_id.as_deref() {
            manager.record_event(
                owner,
                ek::CANDIDATE_VERIFIED,
                json!({
                    "candidate_id": candidate.id,
                    "head_sha": candidate.head_sha,
                    "reused": report.reused,
                }),
            );
        }
    }
    // A check the image has no tool for establishes nothing about the
    // candidate. It is reported as the environment's failing, with the image
    // named, and no `review_rejected` is recorded: the agent is not asked to
    // fix a repository that is not broken. Every not-runnable check is
    // reported at once, so one submission surfaces every missing tool rather
    // than one per round trip.
    let unrunnable: Vec<&review::checks::CheckResult> = report
        .results
        .iter()
        .filter(|result| result.outcome == "not_runnable")
        .collect();
    if !unrunnable.is_empty() {
        for result in &unrunnable {
            manager.record_event(
                session_id,
                ek::CHECK_NOT_RUNNABLE,
                json!({
                    "candidate_id": candidate.id,
                    "command": result.command,
                    "missing_tool": result.missing_tool,
                    "image": report.image,
                    "image_source": report.image_source,
                    "tail": result.tail,
                }),
            );
        }
        let each = unrunnable
            .iter()
            .map(|result| not_runnable_line(result, &report))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "{each}\n\nNothing was verified. Tell the operator; do not change the code to work \
             around a missing tool."
        ));
    }
    if let Some(failed) = report.results.iter().find(|result| !result.ok) {
        let reason = format!(
            "check {}: `{}` (exit {}). Fix it and submit again.\n\n{}",
            failed.outcome,
            failed.command,
            failed
                .exit
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".into()),
            review::output::with_failures(&failed.failures, &failed.tail)
        );
        manager.record_event(
            session_id,
            ek::REVIEW_REJECTED,
            json!({
                "candidate_id": candidate.id,
                "reason": format!("check {}: {}", failed.outcome, failed.command),
                "command": failed.command,
            }),
        );
        return Err(reason);
    }
    Ok(report)
}

/// One check that could not run, said the way its cause reads. A bare name is
/// a tool the image lacks, which is the operator's to add. A path into the
/// workspace is different: no image can supply it, so it is either something
/// preparation should have installed or something the change itself removed —
/// and only the second is the agent's to fix.
fn not_runnable_line(
    result: &review::checks::CheckResult,
    report: &review::checks::CheckReport,
) -> String {
    let tool = result.missing_tool.as_deref().unwrap_or("its command");
    if tool.contains('/') && !tool.starts_with('/') {
        return format!(
            "check `{}` is not runnable here: `{tool}` does not exist in the checked copy of the \
             workspace. If your change removed or renamed it, fix that and submit again. \
             Otherwise it is a dependency this repository's preparation did not install \
             (`prepare` in its `[[repo]]` entry), which is the environment and not your change.",
            result.command,
        );
    }
    format!(
        "check `{}` is not runnable here: `{tool}` is not installed in the check image ({}, {}). \
         This is the environment, not your change.",
        result.command, report.image, report.image_source,
    )
}

/// A fresh session that reads only the requirements and the diff, when the
/// channel binds a model for it (`phases.review.model`). Its verdict lands on
/// the review card; the human still decides. Returns what happened, for the
/// submitting agent's information.
async fn spawn_review_session(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    review_id: &str,
    implementing: Option<&crate::store::SessionRow>,
) -> Value {
    let Some(implementing) = implementing else {
        return json!({ "state": "none", "reason": "a review session reads the submitting session's workspace; a harness you run yourself has none" });
    };
    let bindings = manager.bindings(&ctx.channel);
    let Some(model) = bindings["phases"]["review"]["model"]
        .as_str()
        .filter(|m| !m.trim().is_empty())
        .map(str::to_string)
    else {
        return json!({ "state": "none", "reason": "no review model bound on this channel (phases.review.model)" });
    };
    let Ok(Some(r)) = store.get_review(review_id) else {
        return json!({ "state": "none", "reason": "review not found" });
    };
    let source = match manager.snapshot_workspace(&implementing.id).await {
        Ok(path) => path,
        Err(error) => return json!({ "state": "failed", "reason": error.to_string() }),
    };
    let workspace_id = format!("review-{review_id}");
    if let Err(error) =
        crate::workspace::from_snapshot(manager.backend().as_ref(), &workspace_id, &source).await
    {
        return json!({ "state": "failed", "reason": error.to_string() });
    }
    let short = &review_id[review_id.len().saturating_sub(12)..];
    let spec = crate::session::NewSession {
        channel: ctx.channel.clone(),
        repo_path: String::new(),
        branch: Some(format!("review/{short}")),
        work_item_id: implementing.work_item_id.clone(),
        workspace_id: Some(workspace_id),
        parent_session: None,
        continued_from: None,
        harness: None,
        on_exhaustion: None,
        model,
        budget_tokens: bindings["phases"]["review"]["budget_tokens"].as_i64(),
        initial_prompt: None,
        node_id: None,
        phase: crate::session::Phase::Review,
        review_id: Some(review_id.to_string()),
        base_sha: Some(r.head_sha.clone()),
    };
    match manager.create(spec).await {
        Ok(row) => {
            let _ = store.set_review_session(review_id, &row.id);
            manager.publish_queue().await;
            json!({ "state": "started", "session_id": row.id })
        }
        Err(e) => json!({ "state": "failed", "reason": e.to_string() }),
    }
}

/// `review_verdict`: a review session's verdict on the review it was
/// spawned for. Recorded on the row, never a decision.
async fn verdict(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let session_id = ctx
        .session_id()
        .ok_or("a verdict is given by a review session")?;
    let session = store
        .get_session(session_id)
        .map_err(|e| e.to_string())?
        .ok_or("this session is gone")?;
    if session.phase != "review" {
        return Err(
            "only a review session gives a verdict; submit_review is what an execute session calls"
                .into(),
        );
    }
    let review_id = session
        .review_id
        .clone()
        .ok_or("this review session has no review")?;
    let verdict = str_arg(args, "verdict")?;
    if verdict != "approve" && verdict != "request_changes" {
        return Err(format!("{verdict:?} is not a verdict"));
    }
    let summary = str_arg(args, "summary")?;
    let findings = args.get("findings").cloned().unwrap_or_else(|| json!([]));
    let v = json!({
        "verdict": verdict, "summary": summary, "findings": findings,
        "model": session.model, "session_id": session_id, "at_ms": now_ms(),
    });
    if !store
        .set_ai_verdict(&review_id, &v.to_string())
        .map_err(|e| e.to_string())?
    {
        let state = store
            .get_review(&review_id)
            .map_err(|e| e.to_string())?
            .map(|review| review.state);
        manager.record_event(
            session_id,
            ek::LATE_REFUSED,
            json!({
                "what": "review_verdict",
                "review_id": review_id,
                "state": state,
                "verdict": verdict,
            }),
        );
        manager.phase_done(session_id).await;
        return Err(match state {
            Some(state) => format!(
                "this review was already decided ({state}); a verdict now would be about a \
                 decision that has already been made"
            ),
            None => "the review is gone".into(),
        });
    }
    manager.record_event(
        session_id,
        ek::REVIEW_VERDICT,
        json!({ "review_id": review_id, "verdict": verdict, "summary": summary }),
    );
    manager.publish_queue().await;
    manager.phase_done(session_id).await;
    Ok(json!({ "review_id": review_id, "recorded": true }))
}

/// Poll a narrative report without implying that it could publish anything.
async fn report_status(
    store: &Arc<Store>,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let id = str_arg(args, "report_id")?;
    let wait = wait_secs(args);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);

    loop {
        let report = store
            .get_review(&id)
            .map_err(|e| e.to_string())?
            .ok_or("no report with that id")?;
        if !owns(store, ctx, &report) || report.node_id != ctx.node_id {
            return Err("that report belongs to another session, channel, or node".into());
        }
        if report.kind != crate::store::reports::KIND {
            return Err("report_id names a code review; call review_status instead".into());
        }
        match report.state.as_str() {
            crate::store::reports::ACKNOWLEDGED => {
                return Ok(json!({
                    "report_id": report.id,
                    "state": "acknowledged",
                    "note": report.verdict_reason,
                    "message": "The operator acknowledged this report. This records receipt only; no code was published.",
                }));
            }
            "revising" => {
                return Ok(json!({
                    "report_id": report.id,
                    "state": "changes_requested",
                    "notes": report.verdict_reason,
                    "message": "The operator requested changes. Revise the narrative and call submit_report with this report_id; do not submit code for publication.",
                }));
            }
            _ if tokio::time::Instant::now() >= deadline => {
                return Ok(json!({
                    "report_id": report.id,
                    "state": report.state,
                    "still_waiting": true,
                    "waited_secs": wait,
                    "message": format!(
                        "Still waiting on an operator acknowledgement. The wait is capped at \
                         {MAX_WAIT_SECS} seconds; call report_status again with the same report_id."
                    ),
                }));
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
}

async fn status(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let id = str_arg(args, "review_id")?;
    let wait = wait_secs(args);
    // tokio's clock, not std's, so the wait is the same thing a test can drive.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);

    // Checks the node is running for this handle come first: until they
    // settle, the review either does not exist yet or is still the revision
    // they are meant to replace.
    if let Some(entry) = manager
        .checking()
        .get(&id)
        .filter(|entry| Some(entry.session_id.as_str()) == ctx.session_id())
    {
        match entry.wait(deadline).await {
            None => {
                let mut response = checking(&entry);
                response["still_waiting"] = json!(true);
                response["waited_secs"] = json!(wait);
                response["message"] = json!(format!(
                    "Still running the required checks ({}s so far); nothing reaches the \
                     operator until they pass. The wait is capped at {MAX_WAIT_SECS} seconds; \
                     call review_status again with the same review_id.",
                    entry.running_secs()
                ));
                return Ok(response);
            }
            // What the submission would have refused with, had it waited —
            // unless the review it would have revised was decided since.
            Some(Err(reason)) if !entry.recorded() => {
                let decided = store
                    .get_review(&id)
                    .map_err(|e| e.to_string())?
                    .is_some_and(|r| matches!(r.state.as_str(), "approved" | "rejected"));
                if !decided {
                    return Ok(json!({
                        "review_id": id,
                        "state": "submit_failed",
                        "candidate_id": entry.candidate_id,
                        "checks": entry.commands,
                        "message": reason,
                    }));
                }
            }
            Some(_) => {}
        }
    }

    loop {
        let r = store
            .get_review(&id)
            .map_err(|e| e.to_string())?
            .ok_or("no review with that id")?;
        if !owns(store, ctx, &r) {
            return Err("that review belongs to another session".into());
        }
        if r.kind == crate::store::reports::KIND {
            return Err(
                "that id is a standalone narrative report; call report_status instead".into(),
            );
        }
        match r.state.as_str() {
            "revising" => {
                // A patch means the operator edited the diff rather than
                // describing the change. Applying it verbatim is the point:
                // it is what they want, and the agent is still the hand that
                // writes it to the worktree.
                let message = if r.revision_patch.is_some() {
                    "Changes were requested, with an edited diff. Apply `patch` to the \
                     worktree exactly as given (`git apply`), make any further changes the \
                     notes ask for, commit, then call submit_review again with this \
                     review_id."
                } else {
                    "Changes were requested. Make them, commit, then call submit_review \
                     again with this review_id."
                };
                return Ok(json!({
                    "review_id": r.id,
                    "state": "changes_requested",
                    "notes": r.verdict_reason,
                    "patch": r.revision_patch,
                    "message": message,
                }));
            }
            "approved" | "rejected" => {
                return Ok(json!({
                    "review_id": r.id,
                    "state": r.state,
                    // The approved text, which may not be what was submitted.
                    "title": r.approved_title(),
                    "body": r.approved_body(),
                    "reason": r.verdict_reason,
                    "published": r.publish_result,
                }));
            }
            _ if tokio::time::Instant::now() >= deadline => {
                return Ok(json!({
                    "review_id": r.id,
                    "state": r.state,
                    "still_waiting": true,
                    "waited_secs": wait,
                    "message": format!(
                        "Still waiting on a human; the review is {}. The wait is capped at \
                         {MAX_WAIT_SECS} seconds so this call returns before your MCP client \
                         times out — asking for longer will not help. Call review_status again \
                         with the same review_id to keep waiting.",
                        r.state,
                    ),
                }));
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{key} is required"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_arguments_are_named_when_missing() {
        let args = json!({ "title": "  " });
        assert_eq!(str_arg(&args, "title").unwrap_err(), "title is required");
        assert_eq!(str_arg(&args, "body").unwrap_err(), "body is required");
        assert_eq!(str_arg(&json!({"body":"x"}), "body").unwrap(), "x");
    }

    #[test]
    fn the_cap_is_what_the_tool_description_promises() {
        let def = definitions()
            .into_iter()
            .find(|d| d["name"] == STATUS)
            .expect("review_status is defined");
        let cap = MAX_WAIT_SECS.to_string();
        assert!(def["description"].as_str().unwrap().contains(&cap));
        assert!(def["inputSchema"]["properties"]["wait_secs"]["description"]
            .as_str()
            .unwrap()
            .contains(&cap));
    }
}
