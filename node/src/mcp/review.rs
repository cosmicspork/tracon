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
    store::{
        candidate_id, now_ms, CandidateRow, ReviewRevisionRow, ReviewRow, Store, VerdictRecorded,
    },
};

pub const SUBMIT: &str = "submit_review";
pub const STATUS: &str = "review_status";
pub const SUBMIT_REPORT: &str = "submit_report";
pub const REPORT_STATUS: &str = "report_status";
pub const VERDICT: &str = "review_verdict";

/// What a reviewer's verdict can rest on, most to least: checks it ran
/// itself, evidence the node gave it, or the diff alone.
pub const BASES: [&str; 3] = ["ran_checks", "cited_evidence", "diff_only"];

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
                 from the worktree, so commit first. In a session the node then runs the \
                 repository's required checks, waiting up to `wait_secs` ({MAX_WAIT_SECS} at \
                 most); checks that take longer run on while this returns `state: \"checking\"` \
                 and the review_id review_status waits on. Nothing is published until a human \
                 approves. Resubmit with the same review_id after changes, including after \
                 checks failed; the same commit again while its checks run returns the same \
                 review. A harness you run yourself passes `worktree`: the absolute path of a \
                 worktree whose repository is under the node's [external] repo_roots; its branch \
                 is what gets pushed. Without `change`, approval opens a new pull/merge request \
                 described by `forge.description`, or else by `title` and `body`. With `change`, \
                 approval pushes to that change's branch and sends only what `forge` asks for. \
                 Whatever reaches the forge is read by the repository's own reviewers, who have \
                 never heard of this node: write it for them (what changed, why, trade-offs, \
                 follow-ups, how it was verified), do not address the operator, and do not name \
                 tracon, the node, its tools, or where evidence is attached. What is for the \
                 operator alone goes in `note`, shown on the review beside the diff; a \
                 screenshot, a page or files go to `show_work`. Neither reaches the forge."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "worktree": { "type": "string", "description": "Only for a harness you run yourself: the absolute path of the worktree to review." },
                    "title": { "type": "string", "description": "The change's title, as it should appear." },
                    "body": { "type": "string", "description": "What is not obvious from the diff: intent, trade-offs, follow-ups, verification. The pull/merge request's description unless `forge.description` gives one, so written for the repository's reviewers, not the operator." },
                    "note": { "type": "string", "description": "What you changed, what you verified, what you did not, and anything you want checked. Markdown, for the operator on the review at this commit; pass a new one with each resubmission." },
                    "provider": { "type": "string", "enum": ["github", "gitlab"], "description": "In a session on a GitHub or GitLab repository, defaults to that repository's forge; required outside a session." },
                    "project": { "type": "string", "description": "owner/name on GitHub, the project path on GitLab. In a session on a GitHub or GitLab repository, defaults to that repository; required outside a session. Give both or neither when publishing somewhere else." },
                    "base": { "type": "string", "description": "Branch to merge into. Defaults to the branch the worktree was created from, or on a resubmission to the base it was last submitted with. Naming another base on a resubmission moves the change's base." },
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
                    "change": { "type": "integer", "description": "The open pull request number or merge request iid this branch already has (pr_for_branch or mr_for_branch finds it), to push to instead of opening a new one. Fixed by a review's first submit; a review that opened its change needs no `change`." },
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
                    "message": { "type": "string", "description": "The commit message the change ships with; the title, body and forge description never become it. Omitted, it is your one commit's own message, and that commit ships as it is, SHA and signature, when it sits directly on the base; with several commits beyond the base, `message` is required. Another message ships one new, unsigned commit of the reviewed tree (a repository set to keep its commits pushes them as written). The operator may edit it; the result says what ships (`ships`, `commit_message`, `drops_signatures`)." },
                    "branch": { "type": "string", "description": "The forge branch a new change is pushed to, e.g. feat/short-description. Defaults to your worktree's branch, or one named from the title. Fixed by a review's first submit; an existing change is pushed to its own branch." },
                    "rewrite": { "type": "boolean", "description": "The branch's history was rewritten (rebase, amend), so the push replaces what the forge's branch holds, only over what it held at submit and always approved by the operator." },
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
                "Wait for a review's verdict and return it: `still_waiting` with the current \
                 state while the review is undecided. A submission whose required checks are \
                 still running reads `still_waiting` with `state: \"checking\"` and how long \
                 they have run; if they fail it returns `state: \"submit_failed\"` with the \
                 same message submit_review would have refused with, and nothing was \
                 submitted. While it waits, `reviewer` says what an AI reviewer of the \
                 latest revision is doing (`state`: starting, running, done, failed or none) \
                 and, once it has given one, its `verdict`, `summary` and `findings`; a \
                 verdict arriving ends the wait early. That verdict is advisory: only the \
                 operator's decision publishes. On approval the node publishes the approved \
                 text itself and returns where it landed; if that fails, it reads \
                 `state: \"publish_failed\"` with the error under `publication`, back with \
                 the operator to retry or reject. {}",
                 super::wait::pointer()
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
            "description": format!(
                "Wait for a standalone report's operator decision. It never publishes code. An \
                 acknowledgement records receipt only; requested changes returns notes for a \
                 new submit_report with the same report_id. {}",
                super::wait::pointer()
            ),
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
                    "basis": { "type": "string", "enum": BASES, "description": "What the verdict rests on: `ran_checks` when you ran the repository's checks or tests yourself, `cited_evidence` when it rests on the node's check results in your orientation (shown work is the submitter's own account, not evidence on its own), `diff_only` when you had nothing but the diff. The operator reads it beside your verdict." },
                    "summary": { "type": "string", "description": "Two or three sentences: what the change does and whether it meets the requirements. Say so when the verdict is unverified." },
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
                "required": ["verdict", "basis", "summary"],
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
        REPORT_STATUS => report_status(store, manager, ctx, args).await,
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
    // A submission starts required checks the restart would then have to
    // wait on or cut off; it is refused, and made again once the node is back.
    if let Some(why) = manager.restart().refusal() {
        return Err(format!(
            "submit_review is not taken while {}; submit again then",
            why.trim_start_matches("the ")
        ));
    }
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
    let note = super::show::note_arg(args)?;
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
                // A retry with a note the review does not hold yet keeps it:
                // the words that make this the same submission are the
                // forge's, and the note is not one of them.
                if let Some(note) = note.as_deref() {
                    let held = store
                        .shown_work_for_review(&existing)
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .any(|shown| {
                            shown.row.head_sha == head_sha
                                && shown.row.title == super::show::NOTE_TITLE
                                && shown.row.markdown == note
                        });
                    if !held {
                        super::show::record_note(
                            store,
                            manager,
                            ctx,
                            &existing.id,
                            &head_sha,
                            note,
                        )?;
                    }
                }
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
    // change introduces over what it will merge into. A repository with nothing
    // on the forge yet has no such ref, and everything the worktree holds is
    // the change.
    let range_base = review::range_base(&worktree, &base)
        .await
        .map_err(|e| e.to_string())?;
    let capture = review::capture_over(&worktree, range_base.as_deref(), &branch)
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

    // The cap, before anything else: complexity accretes because nothing says
    // no at submission time. A resubmission is capped the same way.
    let limits = manager.cfg().live().review.clone();
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
    // The repository an external review answers to, for its reviewer and the
    // checks the node runs on its candidate.
    let external_repo = |provider: &str, project: &str| {
        let repo = repo_path.as_deref().filter(|_| external)?;
        let forge_env = Provider::parse(provider)
            .and_then(|forge| {
                manager
                    .broker()
                    .read()
                    .unwrap()
                    .env_for(forge.credential(), &ctx.channel, &ctx.node_id)
                    .ok()
            })
            .unwrap_or_default();
        Some(
            crate::environment::external_review_repo(
                manager.cfg(),
                repo,
                provider,
                project,
                &forge_env,
            )
            .to_string_lossy()
            .into_owned(),
        )
    };
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
            // A review first submitted before the repository was recorded
            // gets it now, rather than never.
            let repo = match target.repo {
                Some(repo) => Some(repo),
                None => external_repo(&target.provider, &target.project),
            };
            Target {
                base: base.clone(),
                repo,
                ..target
            }
        }
        None => Target {
            provider: provider.clone(),
            project: project.clone(),
            base: base.clone(),
            branch: branch.clone(),
            worktree: external.then(|| worktree.clone()),
            repo: external_repo(&provider, &project),
            change: asked_change.map(|number| ChangeRef {
                number,
                url: String::new(),
            }),
            initial: false,
        },
    };
    let mut forge = forge_arg(args)?;
    // The branch is the operator's to rename at approval, not the forge
    // output's; the agent proposes it with `branch`.
    forge.branch = None;
    // Each commit's message is the commit's own; the operator's edits of
    // them are made at approval.
    forge.messages.clear();
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
    } else if resubmission.is_none()
        && (review::prose::placeholder_branch(&target.branch)
            || (asked_change.is_none()
                && pushed_by_another(store, session.as_ref(), &target.branch)))
    {
        if let Some(named) = review::prose::branch_from_title(&title) {
            target.branch = named;
        }
    }
    // With no base on the forge there is nothing for a change to merge into:
    // the only thing approval can do is push the base itself, as the
    // repository's first commit.
    target.initial = range_base.is_none();
    if target.initial {
        initial_push(manager, ctx, &target).await?;
    }
    let (commit_mode, style) = review::prose::rules(
        &manager.cfg().live(),
        store,
        &ctx.channel,
        repo_path.as_deref(),
    );
    // A first commit has nothing to be squashed onto, so it ships as written.
    let squash = commit_mode == crate::config::Commits::Squash && !target.initial;
    let commits = review::commits(&worktree, range_base.as_deref())
        .await
        .map_err(|e| e.to_string())?;
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
    // Who the commits are by, against the account the review publishes as.
    // Named, never rewritten: re-authoring is the agent's or operator's step.
    // On an existing change, only what this submission adds over what the
    // change's branch holds is the submission's to answer for.
    let authorship = review::authorship(
        manager.broker(),
        &provider,
        &ctx.channel,
        &ctx.node_id,
        &worktree,
        range_base.as_deref(),
        &capture.head_sha,
        review::OnChange::of(&target, &intent).as_ref(),
    )
    .await;
    if squash {
        // Onto what the branch holds when the push adds to it; otherwise
        // where the branch leaves its base, so the change is exactly the
        // reviewed diff. Added to what the branch holds, the squash keeps
        // where the branch leaves its base as a second parent unless the
        // branch already has it: a revision that merged its base to resolve
        // a conflict ships that merge. The lease may be an earlier squash
        // this worktree never had; publication settles that case.
        let range_base = range_base.as_deref().expect("a squash has a base");
        let leaves = review::merge_base(&worktree, range_base, &capture.head_sha)
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
    let combined = shipping(&worktree, &capture.head_sha, &mut intent)
        .await
        .map_err(|e| e.to_string())?;
    // A message or branch that breaks the rules fails here, before the card
    // reaches the operator. So does a squash with no message to carry: only
    // the agent's own commit, when there is exactly one, speaks for it.
    let mut broken = Vec::new();
    if squash {
        match review::prose::message_for(&intent.forge, &intent) {
            Some(message) => broken.extend(style.check_message(&message)),
            None if combined > 1 => {
                return Err(format!(
                    "this squashes {combined} of your commits into one, so it needs the message \
                     that commit carries: pass `message`, or make your work one commit"
                ))
            }
            None if combined == 1 => {
                return Err("your commit has no message to ship: pass `message`".into())
            }
            None => {
                return Err(
                    "none of your commits is new to what this squashes onto, so there is no \
                     message of yours to ship: pass `message`"
                        .into(),
                )
            }
        }
    } else {
        for commit in &intent.commits {
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
    let ships = what_ships(&intent);
    let intent_json = Some(serde_json::to_string(&intent).map_err(|e| e.to_string())?);
    let max_snapshot_bytes = manager.cfg().live().supervision.max_snapshot_bytes;
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
        note,
        provider,
        target,
        base,
        capture,
        files,
        intent_json,
        candidate,
        max_snapshot_bytes,
        authorship,
        ships,
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
    with_ships(&mut extras, &submission.ships);
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
            // A session held on these checks hears how they went.
            manager.wait_moved(&session_id).await;
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
        Some(outcome) => {
            // Answered here, so there is nothing for the node to bring back.
            manager
                .waits()
                .tell(session_id, &crate::session::waits::checks_key(&entry));
            if let Ok(Some(review)) = store.get_review(&entry.handle) {
                told_review(manager, ctx, &review);
            }
            outcome
        }
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
    /// The operator's note, kept as shown work once the review is recorded.
    note: Option<String>,
    provider: String,
    target: Target,
    base: String,
    capture: review::Capture,
    files: String,
    intent_json: Option<String>,
    candidate: CandidateRow,
    max_snapshot_bytes: u64,
    authorship: Option<Value>,
    /// What an approval as submitted ships, from `what_ships`.
    ships: Value,
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
            note,
            provider,
            target,
            base,
            capture,
            files,
            intent_json,
            candidate,
            max_snapshot_bytes,
            authorship,
            ships,
        } = self;
        let external = session.is_none();
        let source = if external {
            Source::External
        } else {
            Source::Session
        };
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
            if let Some(note) = note.as_deref() {
                // Before the reviewer starts, which reads shown work; a note that
                // cannot be stored does not undo the review it came with.
                if let Err(error) =
                    super::show::record_note(store, manager, ctx, &id, &capture.head_sha, note)
                {
                    tracing::warn!(%error, review = %id, "submitter's note not stored");
                }
            }
            let reviewer = keep_and_review(
                store,
                manager,
                &id,
                &revision.id,
                &ctx.channel,
                source,
                &worktree,
                &candidate,
                max_snapshot_bytes,
                external,
            )
            .await;
            let mut resubmitted = json!({
                "review_id": id,
                "state": "new",
                "message": format!(
                    "Resubmitted. {} Call review_status to wait for the verdict.",
                    reviewer_says(&reviewer, &ctx.channel)
                ),
                "files": capture.files.len(),
                "added": capture.added,
                "removed": capture.removed,
                "uncommitted": capture.uncommitted,
                "review_session": reviewer,
                "candidate_id": candidate.id,
                "checks_reused": checks.reused,
                "authorship": authorship,
            });
            with_ships(&mut resubmitted, &ships);
            return Ok(resubmitted);
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
            publication_failed: None,
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
        if let Some(note) = note.as_deref() {
            // Before the reviewer starts, which reads shown work; a note that
            // cannot be stored does not undo the review it came with.
            if let Err(error) =
                super::show::record_note(store, manager, ctx, &id, &row.head_sha, note)
            {
                tracing::warn!(%error, review = %id, "submitter's note not stored");
            }
        }
        // The branch the review pushes to, named in place of the node's
        // placeholder, is the session's from here on: its row, the task's
        // workspace and every peer mirroring it show where the work went.
        if let Some(session) = session
            .as_ref()
            .filter(|s| s.branch != target.branch && review::prose::placeholder_branch(&s.branch))
        {
            match store.rename_session_branch(&session.id, &session.branch, &target.branch) {
                Ok(true) => {
                    if let Ok(Some(renamed)) = store.get_session(&session.id) {
                        manager
                            .bus()
                            .publish(crate::stream::Frame::Session(Box::new(renamed)));
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, session = %session.id, "session branch not renamed")
                }
            }
        }
        let reviewer = keep_and_review(
            store,
            manager,
            &id,
            &revision.id,
            &ctx.channel,
            source,
            &worktree,
            &candidate,
            max_snapshot_bytes,
            external,
        )
        .await;

        let mut submitted = json!({
            "review_id": id,
            "state": "new",
            "message": format!(
                "Submitted for review. Nothing is published until a human approves. {} Call \
                 review_status to wait for the verdict.",
                reviewer_says(&reviewer, &ctx.channel)
            ),
            "files": capture.files.len(),
            "added": row.added,
            "removed": row.removed,
            "uncommitted": capture.uncommitted,
            "review_session": reviewer,
            "candidate_id": candidate.id,
            "checks_reused": checks.reused,
            "authorship": authorship,
        });
        with_ships(&mut submitted, &ships);
        Ok(submitted)
    }
}

/// `what_ships`'s fields, added to a submission's result.
fn with_ships(result: &mut Value, ships: &Value) {
    if let (Some(result), Some(ships)) = (result.as_object_mut(), ships.as_object()) {
        result.extend(ships.clone());
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
/// refuses. Returns the read when one was started, for what must wait on it,
/// which comes to why the files were not kept if they were not, and with
/// `hand_back` the files it read, for a caller about to build the tree from
/// them that need not read them back out of the store.
fn retain_candidate_files(
    store: &Arc<Store>,
    worktree: &str,
    candidate: &CandidateRow,
    max_snapshot_bytes: u64,
    hand_back: bool,
) -> Option<Kept> {
    let materialized = serde_json::from_str::<Value>(&candidate.capture_json)
        .ok()
        .and_then(|capture| capture["materialized"].as_bool())
        .unwrap_or(false);
    if materialized {
        return None;
    }
    let store = store.clone();
    let worktree = worktree.to_string();
    let id = candidate.id.clone();
    let head_sha = candidate.head_sha.clone();
    Some(tokio::spawn(async move {
        let retained = match review::read_candidate(&worktree, &head_sha, max_snapshot_bytes).await
        {
            Ok(tree) => tokio::task::spawn_blocking(move || {
                store.insert_candidate_files(&id, &tree.files)?;
                store.record_candidate_materialized(&id)?;
                Ok::<_, crate::store::StoreError>(hand_back.then_some(tree.files))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|stored| stored.map_err(|error| error.to_string())),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = &retained {
            tracing::warn!(%error, head_sha, "could not keep a candidate's files");
        }
        retained
    }))
}

/// A read of a candidate's files under way, and the files when they were
/// asked for.
type Kept = tokio::task::JoinHandle<Result<Option<Vec<crate::store::CandidateFile>>, String>>;

/// What follows a submission or a resubmission once its review is recorded:
/// the candidate's files kept, when they are not yet; the node's checks run on
/// them, for a harness the operator runs with `[external] run_checks` on; and
/// a reviewer started once both are done. What became of the reviewer.
#[allow(clippy::too_many_arguments)]
async fn keep_and_review(
    store: &Arc<Store>,
    manager: &Manager,
    review_id: &str,
    revision_id: &str,
    channel: &str,
    source: Source,
    worktree: &str,
    candidate: &CandidateRow,
    max_snapshot_bytes: u64,
    external: bool,
) -> Value {
    // The same commit submitted again while its checks run gets those
    // checks, not a second run of them (`ExternalChecks::begin`).
    let check = (external && manager.cfg().external.run_checks)
        .then(|| manager.external_checks().begin(store, review_id, candidate))
        .flatten();
    let checking = check.is_some();
    let retained = retain_candidate_files(store, worktree, candidate, max_snapshot_bytes, checking);
    let waited = match check {
        Some(check) => Some(check_external_candidate(
            store, manager, review_id, candidate, retained, check,
        )),
        None => retained,
    };
    // Publishes the queue itself, once a start by hand would find what to
    // wait on.
    spawn_review_session_when_kept(
        store,
        manager,
        review_id,
        revision_id,
        channel,
        source,
        candidate,
        waited,
        checking,
    )
    .await
}

/// For a review from a harness the operator runs, with `[external]
/// run_checks` on: the repository's required checks, run on the node's own
/// copy of the candidate once `retained` has kept its files, as evidence for
/// the reviewer, who starts once they are done. They run behind the
/// submission and gate nothing; their results land on the candidate as an
/// internal session's do, and on the review card, which says they are
/// running until then. Whether a submission is external is the caller's to
/// say: a candidate is named by its commit, so its owner is whoever captured
/// that commit first, not who submitted it now. Returns what a reviewer waits
/// on, which comes to why the files were not kept if they were not; `running`
/// is held until the checks are done.
fn check_external_candidate(
    store: &Arc<Store>,
    manager: &Manager,
    review_id: &str,
    candidate: &CandidateRow,
    retained: Option<Kept>,
    running: ExternalCheck,
) -> Kept {
    say_checks_running(store, review_id, &candidate.head_sha);
    let store = store.clone();
    let manager = manager.clone();
    let review_id = review_id.to_string();
    let candidate = candidate.clone();
    tokio::spawn(async move {
        let kept = match retained {
            Some(retained) => retained
                .await
                .map_err(|error| error.to_string())
                .and_then(|kept| kept),
            None => Ok(None),
        };
        let (ran, kept) = match kept {
            Ok(mut files) => {
                let ran = loop {
                    let ran = check_kept_candidate(
                        &store,
                        &manager,
                        &review_id,
                        &candidate,
                        files.take(),
                    )
                    .await;
                    // Stopped because the review moved to another commit,
                    // and back to this one meanwhile: that submission
                    // attached to this run, so it runs again for it.
                    let again = matches!(ran, Ok(None))
                        && running.settle(|| {
                            store
                                .review_undecided_at(&review_id, &candidate.head_sha)
                                .unwrap_or(false)
                        });
                    if !again {
                        break ran;
                    }
                };
                (ran, Ok(None))
            }
            Err(why) => (
                Err(format!("the candidate's files could not be kept: {why}")),
                Err(why),
            ),
        };
        // Settled before anything is written: a submission attaching from
        // here on starts its own run, and one that attached before has the
        // running state it wrote replaced below.
        running.settle(|| false);
        match ran {
            // Written only onto the commit they ran on, in the same
            // statement that asks: a resubmission landing meanwhile keeps
            // its own.
            Ok(Some(results)) => {
                if let Err(error) = store.set_checks_at(&review_id, &candidate.head_sha, &results) {
                    tracing::warn!(%error, review_id, "could not record an external candidate's checks");
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, review_id, "could not check an external candidate");
                // Said where the card and the reviewer read checks, so that
                // the node failing to run them never reads as there being
                // none.
                if let Err(error) =
                    record_unrun_checks(&store, &review_id, &candidate.head_sha, &error)
                {
                    tracing::warn!(%error, review_id, "could not record why no check ran");
                }
            }
        }
        // Checks stopped because the review was decided leave nothing
        // running on its card.
        if let Err(error) =
            store.clear_checks_if(&review_id, &candidate.head_sha, &running_checks())
        {
            tracing::warn!(%error, review_id, "could not clear checks that stopped");
        }
        manager.publish_queue().await;
        drop(running);
        // The reviewer builds its workspace from the store, as ever.
        kept
    })
}

/// What a review's checks record says while the node runs them: one entry,
/// [`CHECKS_RUNNING`], which its results replace.
fn running_checks() -> String {
    let entry = review::checks::CheckResult {
        command: "required checks".into(),
        ok: false,
        exit: None,
        outcome: CHECKS_RUNNING.into(),
        tail: "the node is running this candidate's required checks".into(),
        failures: String::new(),
        ms: 0,
        reused_from: None,
        missing_tool: None,
    };
    serde_json::to_string(&[entry]).unwrap_or_default()
}

/// The outcome [`running_checks`] records.
pub const CHECKS_RUNNING: &str = "running";

/// Record on the review that its checks are running, on the commit they run
/// on.
fn say_checks_running(store: &Store, review_id: &str, head_sha: &str) {
    if let Err(error) = store.set_checks_at(review_id, head_sha, &running_checks()) {
        tracing::warn!(%error, review_id, "could not record that checks are running");
    }
}

/// The node's checks of external candidates under way on this node, by
/// review and candidate, so that a second submission of a commit whose
/// checks are running does not run them again beside the first. Counted in
/// what a restart would interrupt (`restart::busy`).
#[derive(Default)]
pub struct ExternalChecks(std::sync::Mutex<std::collections::HashSet<(String, String)>>);

impl ExternalChecks {
    /// Begin checking a review's candidate, or `None` when a run of it is
    /// under way: that run's results are this submission's too, and the
    /// review says they are running again, which its resubmission cleared.
    /// Said under the same lock a run settles under, so a run that has
    /// settled is never said to be running: a submission after it starts
    /// its own.
    pub fn begin(
        self: &Arc<Self>,
        store: &Store,
        review_id: &str,
        candidate: &CandidateRow,
    ) -> Option<ExternalCheck> {
        let key = (review_id.to_string(), candidate.id.clone());
        let mut running = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if running.contains(&key) {
            say_checks_running(store, review_id, &candidate.head_sha);
            return None;
        }
        running.insert(key.clone());
        Some(ExternalCheck {
            checks: self.clone(),
            key,
            settled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Whether a run of a review's candidate is under way.
    pub fn running(&self, review_id: &str, candidate_id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(review_id.to_string(), candidate_id.to_string()))
    }

    /// Runs under way.
    pub fn in_flight(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// A check under way; settling or dropping it lets the next one through.
pub struct ExternalCheck {
    checks: Arc<ExternalChecks>,
    key: (String, String),
    /// Once settled, the key is no longer this run's: a run of the same
    /// commit begun since holds it, and nothing this guard does may remove
    /// it.
    settled: std::sync::atomic::AtomicBool,
}

impl ExternalCheck {
    /// End the run unless `again` says, under the lock submissions attach
    /// under, that it must run again. Whether it must. Only the first
    /// settle that ends it does anything.
    fn settle(&self, again: impl FnOnce() -> bool) -> bool {
        use std::sync::atomic::Ordering;
        let mut running = self.checks.0.lock().unwrap_or_else(|e| e.into_inner());
        if self.settled.load(Ordering::Acquire) {
            return false;
        }
        if again() {
            return true;
        }
        running.remove(&self.key);
        self.settled.store(true, Ordering::Release);
        false
    }
}

impl Drop for ExternalCheck {
    fn drop(&mut self) {
        self.settle(|| false);
    }
}

/// [`check_external_candidate`]'s run, on a tree built from `files` as they
/// were just read, or from the kept files when there are none in hand: the
/// review's checks record, or `None` when the run stopped because the review
/// no longer wants it.
async fn check_kept_candidate(
    store: &Arc<Store>,
    manager: &Manager,
    review_id: &str,
    candidate: &CandidateRow,
    files: Option<Vec<crate::store::CandidateFile>>,
) -> Result<Option<String>, String> {
    let review = store
        .get_review(review_id)
        .map_err(|error| error.to_string())?
        .ok_or("the review is gone")?;
    let repo = crate::environment::review_repo(store, &review)?;
    let files = match files {
        Some(files) => files,
        None => store
            .candidate_files(&candidate.id)
            .map_err(|error| error.to_string())?,
    };
    let snapshot = candidate_tree(candidate, files, false).await?;
    let report = review::checks::run_required_for(
        manager.backend().as_ref(),
        manager.cfg(),
        store,
        candidate,
        // The node's own run, for no session: whoever first captured this
        // commit did not ask for it.
        None,
        repo.as_deref(),
        &snapshot.root,
        false,
        &ReviewStillWants {
            store: store.clone(),
            review_id: review_id.to_string(),
            head_sha: candidate.head_sha.clone(),
        },
    )
    .await;
    tokio::task::spawn_blocking(move || drop(snapshot));
    let mut report = report?;
    if report.cancelled.is_some() {
        return Ok(None);
    }
    // A failed preparation ran no check. Each check it stopped says so in a
    // line, and what preparation said is told once, as its own entry, the
    // way a session's refusal says it: not copied under every check.
    if let Some(failure) = report
        .prepared
        .as_ref()
        .and_then(|prepared| prepared.failure.as_ref())
    {
        tracing::info!(review_id, reason = %failure.reason(), "an external candidate's preparation failed");
        report.results.insert(
            0,
            review::checks::CheckResult {
                command: PREPARE.into(),
                ok: false,
                exit: failure.exit,
                outcome: "prepare_failed".into(),
                // What preparation said is bounded as a check's output is:
                // a long log is kept to where it ended.
                tail: format!(
                    "{}, in {} ({}).\n\n{}",
                    failure.reason(),
                    report.image,
                    report.image_source,
                    review::checks::tail(&failure.to_string())
                ),
                failures: String::new(),
                ms: report.prepared.as_ref().map_or(0, |prepared| prepared.ms),
                reused_from: None,
                missing_tool: None,
            },
        );
    }
    serde_json::to_string(&report.results)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// What a review's checks record names preparation by, when it failed and
/// ran none of them.
const PREPARE: &str = "prepare";

/// What a review's checks record says when the node could not run them at
/// all: one entry, `could_not_run`, with why, written onto the commit they
/// were for. Whether it was written.
fn record_unrun_checks(
    store: &Store,
    review_id: &str,
    head_sha: &str,
    why: &str,
) -> Result<bool, String> {
    store
        .set_checks_at(review_id, head_sha, &unrun_checks(why))
        .map_err(|error| error.to_string())
}

/// The one-entry checks record [`record_unrun_checks`] writes.
fn unrun_checks(why: &str) -> String {
    let entry = review::checks::CheckResult {
        command: "required checks".into(),
        ok: false,
        exit: None,
        outcome: COULD_NOT_RUN.into(),
        tail: format!("the node could not run this candidate's checks: {why}"),
        failures: String::new(),
        ms: 0,
        reused_from: None,
        missing_tool: None,
    };
    serde_json::to_string(&[entry]).unwrap_or_default()
}

/// The outcome [`record_unrun_checks`] records.
pub const COULD_NOT_RUN: &str = "could_not_run";

/// A candidate's tree on disk, built from its kept `files`, for a check run
/// or a reviewer's workspace. `writable_top` leaves its top directory
/// writable, for the repository a session needs there.
async fn candidate_tree(
    candidate: &CandidateRow,
    files: Vec<crate::store::CandidateFile>,
    writable_top: bool,
) -> Result<review::CandidateSnapshot, String> {
    let root = std::env::temp_dir()
        .join("tracon-candidates")
        .join(uuid::Uuid::now_v7().to_string());
    let tree_sha = candidate.tree_sha.clone().unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let snapshot = review::CandidateSnapshot::materialize(root, tree_sha, files)?;
        #[cfg(unix)]
        if writable_top {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&snapshot.root, std::fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(not(unix))]
        let _ = writable_top;
        Ok::<_, review::ReviewError>(snapshot)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

/// The review an external candidate's checks were started for, as the
/// authority on whether they should still run: once it is decided, or
/// resubmitted with another commit, they describe nothing anyone will read.
struct ReviewStillWants {
    store: Arc<Store>,
    review_id: String,
    head_sha: String,
}

impl review::checks::Cancel for ReviewStillWants {
    fn cancelled(&self) -> Option<String> {
        (!self
            .store
            .review_undecided_at(&self.review_id, &self.head_sha)
            .unwrap_or(false))
        .then(|| "the review was decided or moved to another commit".to_string())
    }
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

/// Which of the agent's commits a commit made in publication would replace
/// with their signatures, and under squash the message that ships the head
/// itself, so the operator sees before approving whether what ships is the
/// agent's commit or a new one (#551). Under squash, also how many of the
/// agent's commits the squash combines, and when that is one, its message,
/// which the squash carries when nobody wrote one (#592).
async fn shipping(
    worktree: &str,
    head_sha: &str,
    intent: &mut review::publish::Intent,
) -> Result<usize, review::ReviewError> {
    let mut combined = Vec::new();
    for commit in &intent.commits {
        let raw = review::raw_commit(worktree, &commit.sha).await?;
        // Under squash, a commit already on what the squash sits on is not
        // replaced, and neither is one in what this worktree holds of it.
        let replaced = match &intent.squash_onto {
            Some(onto) => {
                let held = match intent.held.as_deref().filter(|_| !intent.rewrite) {
                    Some(held) => review::descends_from(worktree, held, &commit.sha).await,
                    None => false,
                };
                !held && !review::descends_from(worktree, onto, &commit.sha).await
            }
            None => true,
        };
        if replaced && review::publish::is_signed(&raw) {
            intent.signed.push(commit.sha.clone());
        }
        if replaced && intent.squash_onto.is_some() {
            combined.push(review::publish::message_of(&raw));
        }
        let Some(onto) = intent
            .squash_onto
            .as_deref()
            .filter(|_| commit.sha == head_sha)
        else {
            continue;
        };
        // A head with the tree it sits on is no revision: the squash
        // pushes nothing new, and the head does not ship either.
        let tree = |raw: &[u8]| raw.split(|b| *b == b'\n').next().map(<[u8]>::to_vec);
        let onto_tree = review::raw_commit(worktree, onto).await.ok();
        if onto_tree.as_deref().map(tree) == Some(tree(&raw)) {
            continue;
        }
        intent.as_written =
            review::publish::as_written(&raw, onto, intent.squash_merges.as_deref());
    }
    if let [Some(message)] = &combined[..] {
        intent.own_message = Some(message.clone());
    }
    Ok(combined.len())
}

/// What an approval of this revision as submitted ships, told to the agent
/// in the submission's result so it can resubmit with `message` before the
/// operator decides (#592): `own` when its commits are pushed as they are,
/// `new` when publication makes a commit, the message that commit carries,
/// and the signed commits it replaces, whose signatures are dropped.
fn what_ships(intent: &review::publish::Intent) -> Value {
    if intent.squash_onto.is_none() {
        return json!({ "ships": "own", "commit_message": null, "drops_signatures": [] });
    }
    let message = review::prose::message_for(&intent.forge, intent);
    let own = message.is_some() && message == intent.as_written;
    json!({
        "ships": if own { "own" } else { "new" },
        "commit_message": message,
        "drops_signatures": if own { Vec::new() } else { intent.signed.clone() },
    })
}

/// Whether a review of a repository with nothing on the forge yet can be
/// published: only by pushing its base as the repository's first commit,
/// since a change needs a base that exists to merge into. Where the forge can
/// be asked, it is also asked whether the base is there after all, which a
/// worktree cloned before anything was pushed would not know.
async fn initial_push(manager: &Manager, ctx: &CallContext, target: &Target) -> Result<(), String> {
    let Some(provider) = Provider::parse(&target.provider) else {
        return Ok(());
    };
    let base = &target.base;
    if target.change.is_some() || target.branch != *base {
        return Err(format!(
            "origin has no {base} yet: the repository is empty, so a {} has no {base} to merge \
             into. Push an initial commit to {base} on the forge, fetch it and rebase onto it, \
             then submit again; or submit with branch {base} to push this as the repository's \
             first commit.",
            provider.noun()
        ));
    }
    let env =
        manager
            .broker()
            .read()
            .unwrap()
            .env_for(provider.credential(), &ctx.channel, &ctx.node_id);
    let (Ok(env), Ok(dir)) = (env, review::publish::inspection_dir()) else {
        return Ok(());
    };
    let live = manager.cfg().live();
    match review::publish::branch_head(provider, &live, &dir, &env, target).await {
        Ok(Some(held)) => Err(format!(
            "{base} is on the forge at {held:.8}, but this worktree has no origin/{base}: fetch \
             it and rebase onto it, then submit again"
        )),
        Ok(None) => Ok(()),
        Err(error) => {
            tracing::warn!(%error, "could not ask the forge whether the base exists");
            Ok(())
        }
    }
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
    let live = manager.cfg().live();
    let cfg: &crate::config::Config = &live;
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
    let mut held = builds_on.then(|| lease.clone());
    // What the branch holds may be this node's own earlier squash, which
    // the agent never had: building on the commit that squash was made from
    // is building on it, and that commit is what stands for the branch here.
    if let (false, Some(store)) = (builds_on, squashed_by) {
        let pushed = store
            .publications_that_pushed(&ctx.channel, &target.project, &lease)
            .map_err(|e| e.to_string())?;
        for publication in &pushed {
            let from = &publication.head_sha;
            if from == head_sha || review::descends_from(worktree, head_sha, from).await {
                builds_on = true;
                held = Some(from.clone());
                break;
            }
        }
        // A rewrite builds on none of them, but what the branch held is
        // still named by whichever this worktree has.
        if held.is_none() && !review::holds_commit(worktree, &lease).await {
            for publication in pushed {
                if review::holds_commit(worktree, &publication.head_sha).await {
                    held = Some(publication.head_sha);
                    break;
                }
            }
        }
    }
    if held.is_none() && review::holds_commit(worktree, &lease).await {
        held = Some(lease.clone());
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
        held,
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
        publication_failed: None,
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
    let not_run: Vec<&str> = report
        .results
        .iter()
        .filter(|result| result.outcome == "not_run")
        .map(|result| result.command.as_str())
        .collect();
    if let Some(prepared) = &report.prepared {
        let mut payload =
            json!({ "candidate_id": candidate.id, "ok": prepared.ok, "ms": prepared.ms });
        // A failed preparation is reported once, here, with what failed and
        // which checks it kept from running, rather than as each check's
        // result: none of them ran.
        if let Some(failure) = &prepared.failure {
            payload["reason"] = json!(failure.reason());
            payload["command"] = json!(failure.command);
            payload["exit"] = json!(failure.exit);
            payload["message"] = json!(failure.message);
            payload["tail"] = json!(failure.output);
            payload["not_run"] = json!(not_run);
        }
        manager.record_event(session_id, ek::CHECK_PREPARED, payload);
    }
    for result in report
        .results
        .iter()
        .filter(|result| result.outcome != "not_run")
    {
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
    // A failed preparation ran no check, so the refusal names preparation
    // and what it said, never a check that did not run. Whether the cause is
    // the change (a manifest or lockfile) or the environment (a registry it
    // cannot reach, a credential it lacks) is for its output to tell.
    if let Some(failure) = report
        .prepared
        .as_ref()
        .and_then(|prepared| prepared.failure.as_ref())
    {
        let reason = failure.reason();
        manager.record_event(
            session_id,
            ek::REVIEW_REJECTED,
            json!({
                "candidate_id": candidate.id,
                "reason": reason,
                "stage": "prepare",
                "command": failure.command,
                "exit": failure.exit,
            }),
        );
        let checks = not_run
            .iter()
            .map(|command| format!("`{command}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let said = if failure.output.is_empty() {
            String::new()
        } else {
            format!("\n\n{}", failure.output)
        };
        return Err(format!(
            "{reason}, in {} ({}). No check was run ({checks}), so nothing was verified. If \
             your change broke preparation (a manifest or lockfile), fix that and submit again; \
             if it is the environment (a host it cannot reach, a credential it lacks), tell the \
             operator rather than changing the code to work around it.{said}",
            report.image, report.image_source,
        ));
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
        let reason = failed_check_refusal(failed);
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

/// The refusal for a check that ran and failed. A reused failure is the
/// recorded result of an identical earlier run, so "submit again" would only
/// fetch it again unless the agent asks for a rerun: its advice names
/// `rerun_checks` as the way past a stale record, and what else would change it.
fn failed_check_refusal(failed: &review::checks::CheckResult) -> String {
    let advice = if failed.reused_from.is_some() {
        "This failure is the recorded result of an identical earlier run of this commit; \
         submitting the same commit again returns it again unless you pass \
         `rerun_checks: true`. If you believe the recorded result is stale (a flake, or an \
         environment since fixed), resubmit with `rerun_checks: true`; otherwise change the \
         commit, or if the check itself is wrong, say so to the operator."
    } else {
        "Fix it and submit again."
    };
    format!(
        "check {}: `{}` (exit {}). {advice}\n\n{}",
        failed.outcome,
        failed.command,
        failed
            .exit
            .map(|code| code.to_string())
            .unwrap_or_else(|| "none".into()),
        review::output::with_failures(&failed.failures, &failed.tail)
    )
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

/// Where a review session finds the whole diff in its workspace. The diff in
/// its instructions is cut at a length, and the workspace is the candidate's
/// tree with no Git history to ask for the rest.
pub const REVIEW_DIFF: &str = ".tracon/review.diff";

/// A fresh session that reads the requirements, the diff and the evidence the
/// node holds on the candidate, never how the change was made, when the
/// channel binds a model for it (`phases.review.model`). It runs in the
/// review's repository's image (`environment::review_repo`). Its verdict lands on
/// the review card; the human still decides. Its workspace is the review's
/// immutable candidate, never the submitting session's workspace, so a review
/// from a harness the operator runs gets one too. Why none started is kept on
/// the review. Returns what happened, for the caller's information.
pub(crate) async fn spawn_review_session(
    store: &Arc<Store>,
    manager: &Manager,
    review_id: &str,
    started_by: StartedBy,
) -> Value {
    match manager.review_starts().claim(review_id) {
        Some(start) => spawn_claimed_review_session(store, manager, start, None, started_by).await,
        None => already_starting(),
    }
}

/// [`spawn_review_session`] under a start already claimed, which ends when
/// this does. `unkept` is why the candidate's files were not kept, when
/// keeping them was tried and failed.
pub async fn spawn_claimed_review_session(
    store: &Arc<Store>,
    manager: &Manager,
    start: ReviewStart,
    unkept: Option<String>,
    started_by: StartedBy,
) -> Value {
    let review_id = start.review_id();
    match start_review_session(
        store,
        manager,
        review_id,
        start.head(),
        unkept.as_deref(),
        started_by,
    )
    .await
    {
        Ok(session_id) => json!({ "state": "started", "session_id": session_id }),
        Err((state, reason)) => {
            // The start for the commit it moved to records its own.
            if reason == MOVED {
                return json!({ "state": state, "reason": reason });
            }
            if let Err(error) = store.set_review_session_reason(review_id, &reason) {
                tracing::warn!(%error, review_id, "could not record why no reviewer started");
            }
            json!({ "state": state, "reason": reason })
        }
    }
}

/// What a start says when another is already under way for the review. Its
/// reason is not recorded: the start under way records its own.
fn already_starting() -> Value {
    json!({ "state": "none", "reason": ALREADY_STARTING })
}

/// Why a start was refused while another was under way.
pub const ALREADY_STARTING: &str =
    "a review session is already starting on this review; wait for it to start";

/// Why a start for one commit started nothing: the review was resubmitted
/// with another while it waited, and the start for that one goes ahead.
const MOVED: &str = "the review moved to another commit before its reviewer started";

/// The reviews a review session is being started for on this node, from
/// the claim until the session is recorded on the review or found not to
/// start. Until then `review_session_id` says nothing, and a second start
/// would import into the same `review-{id}` workspace as the first and
/// replace it while the first runs. Node-local: only a review's owner
/// starts one.
/// Each claim says the commit it is for, when it is for one.
#[derive(Default)]
pub struct ReviewStarts {
    pending: std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    /// Woken whenever a claim is let go, for a start waiting to take it over.
    released: tokio::sync::Notify,
    /// Reviews whose latest revision's files are being kept with no start
    /// claimed for them (a manual trigger): a start by hand waits on this
    /// rather than finding them not kept yet. Kept once they were not, so a
    /// later start can say why, until the review is resubmitted or decided.
    keeping: std::sync::Mutex<std::collections::HashMap<String, Keep>>,
    /// Numbers each keep, so an earlier submission's that finishes late
    /// leaves a later one's entry alone.
    keeps: std::sync::atomic::AtomicU64,
}

/// One revision's keep: its number, the revision, and whether its files are
/// kept yet (`None` while they are being kept, then why they were not, if
/// they were not).
struct Keep {
    number: u64,
    revision_id: String,
    kept: tokio::sync::watch::Receiver<Option<Option<String>>>,
}

impl ReviewStarts {
    /// Claim the start for a review, from whatever its latest revision is
    /// then, or `None` while another holds it.
    pub fn claim(self: &Arc<Self>, review_id: &str) -> Option<ReviewStart> {
        self.claim_at(review_id, None)
    }

    /// Claim the start for a review at `head_sha`, which starts nothing once
    /// the review has moved to another commit; `None` while another holds it.
    pub fn claim_at(
        self: &Arc<Self>,
        review_id: &str,
        head_sha: Option<&str>,
    ) -> Option<ReviewStart> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.contains_key(review_id) {
            return None;
        }
        pending.insert(review_id.to_string(), head_sha.map(str::to_string));
        Some(ReviewStart {
            starts: self.clone(),
            review_id: review_id.to_string(),
            head_sha: head_sha.map(str::to_string),
        })
    }

    /// Whether a review session is being started for a review.
    pub fn pending(&self, review_id: &str) -> bool {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.contains_key(review_id)
    }

    /// Whether the start under way for a review is for `head_sha`.
    fn starting_at(&self, review_id: &str, head_sha: &str) -> bool {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.get(review_id).and_then(Option::as_deref) == Some(head_sha)
    }

    /// Follow the keeping of a review's files that no start waits on, so a
    /// start by hand can.
    fn keep(self: &Arc<Self>, review_id: &str, revision_id: &str, retained: Kept) {
        let (tx, rx) = tokio::sync::watch::channel(None);
        let keep = self
            .keeps
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.keeping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                review_id.to_string(),
                Keep {
                    number: keep,
                    revision_id: revision_id.to_string(),
                    kept: rx,
                },
            );
        let starts = self.clone();
        let review_id = review_id.to_string();
        tokio::spawn(async move {
            let unkept = unkept(retained.await);
            let kept = unkept.is_none();
            let _ = tx.send(Some(unkept));
            // Files that were kept need no entry: a start finds them. One
            // that says why they were not stays for a later start to report.
            if kept {
                let mut keeping = starts.keeping.lock().unwrap_or_else(|e| e.into_inner());
                if keeping.get(&review_id).is_some_and(|k| k.number == keep) {
                    keeping.remove(&review_id);
                }
            }
        });
    }

    /// Once a revision's files are kept, if they are being kept now: why
    /// they were not, if they were not. Another revision's keep says
    /// nothing of this one's.
    pub async fn kept(&self, review_id: &str, revision_id: &str) -> Option<String> {
        let keeping = self
            .keeping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(review_id)
            .filter(|keep| keep.revision_id == revision_id)
            .map(|keep| keep.kept.clone());
        let mut keeping = keeping?;
        let done = keeping.wait_for(Option::is_some).await.ok()?;
        done.clone().flatten()
    }
}

impl ReviewStarts {
    /// Forget a review's keep: it was resubmitted, or decided.
    pub fn unkeep(&self, review_id: &str) {
        self.keeping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(review_id);
    }

    /// Forget the keeps of reviews no longer open (`open` holds those that
    /// are), however they were decided.
    pub fn retain_open(&self, open: &std::collections::HashSet<&str>) {
        self.keeping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|review, _| open.contains(review.as_str()));
    }
}

/// Why a candidate's files were not kept, from the task keeping them.
fn unkept<T>(retained: Result<Result<T, String>, tokio::task::JoinError>) -> Option<String> {
    match retained {
        Ok(Ok(_)) => None,
        Ok(Err(why)) => Some(why),
        Err(error) => Some(error.to_string()),
    }
}

/// A claimed start; dropping it lets the next one through.
pub struct ReviewStart {
    starts: Arc<ReviewStarts>,
    review_id: String,
    head_sha: Option<String>,
}

impl ReviewStart {
    pub fn review_id(&self) -> &str {
        &self.review_id
    }

    /// The commit it is for, when it was claimed for one.
    pub fn head(&self) -> Option<&str> {
        self.head_sha.as_deref()
    }
}

impl Drop for ReviewStart {
    fn drop(&mut self) {
        self.starts
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.review_id);
        self.starts.released.notify_waiters();
    }
}

/// What a submission starts of a reviewer, as the channel's trigger for its
/// source says: under `manual` nothing, and the review records why. The
/// result names the trigger and model that applied, for the submitter.
/// It publishes the queue once a start by hand would find the keep it waits
/// on, so the review is never on the page before that.
#[allow(clippy::too_many_arguments)]
async fn spawn_review_session_when_kept(
    store: &Arc<Store>,
    manager: &Manager,
    review_id: &str,
    revision_id: &str,
    channel: &str,
    source: Source,
    candidate: &CandidateRow,
    retained: Option<Kept>,
    checking: bool,
) -> Value {
    // An earlier revision's keep is nothing to this one.
    manager.review_starts().unkeep(review_id);
    let binding = ReviewerBinding::for_channel(manager, channel);
    let trigger = binding.trigger(source);
    // What applied to this revision, whatever the binding says later.
    if let Err(error) =
        store.review_revision_trigger_record(review_id, revision_id, trigger.as_str())
    {
        tracing::warn!(%error, review_id, "could not record the trigger a revision was submitted under");
    }
    let mut started = if binding.model.is_some() && trigger == Trigger::Manual {
        if let Err(error) = store.set_review_session_reason(review_id, MANUAL_REVIEW) {
            tracing::warn!(%error, review_id, "could not record why no reviewer started");
        }
        // The operator may start one before the files are kept; that start
        // waits for them, and for any checks the node runs on them, as one
        // on submission does.
        if let Some(retained) = retained {
            manager
                .review_starts()
                .keep(review_id, revision_id, retained);
        }
        manager.publish_queue().await;
        json!({ "state": "none", "reason": MANUAL_REVIEW })
    } else {
        manager.publish_queue().await;
        spawn_on_submission(
            store,
            manager,
            review_id,
            binding.model.is_some(),
            candidate,
            retained,
            checking,
        )
        .await
    };
    started["trigger"] = json!(trigger.as_str());
    started["source"] = json!(source.as_str());
    started["model"] = json!(binding.model);
    started
}

/// [`spawn_review_session`] once the candidate's files are kept: they are
/// what the reviewer's workspace is built from, and an external harness's are
/// read after its review is recorded (`retain_candidate_files`). `checking`
/// says `retained` also runs the checks the node makes its evidence
/// (`check_external_candidate`). The submitter is not held on either; it
/// hears that a reviewer is starting.
///
/// The start is for `candidate`'s commit. One for an earlier commit, or for
/// this commit with a run of its checks that has since been replaced by
/// this submission's, starts nothing once its own wait is over (`MOVED`), so
/// this one waits on its own and then takes the start over, woken when the
/// other lets go. One that waited out another start does not start a second
/// reviewer on the revision that start already started one on.
async fn spawn_on_submission(
    store: &Arc<Store>,
    manager: &Manager,
    review_id: &str,
    bound: bool,
    candidate: &CandidateRow,
    retained: Option<Kept>,
    checking: bool,
) -> Value {
    let head_sha = candidate.head_sha.as_str();
    let Some(retained) = retained.filter(|_| bound) else {
        return spawn_review_session(store, manager, review_id, StartedBy::Submission).await;
    };
    let starts = manager.review_starts();
    // Claimed now, not once the files are kept: from here until the session
    // is recorded, a start by hand must see this one.
    let claimed = starts.claim_at(review_id, Some(head_sha));
    // A start for this commit already under way starts the reviewer this
    // would, unless this submission's own checks are what it should wait on:
    // that start gives way to them once its own wait is over.
    if claimed.is_none() && !checking && starts.starting_at(review_id, head_sha) {
        return already_starting();
    }
    let reason = if checking {
        WAITING_FOR_CHECKS
    } else {
        "waiting for the candidate's files to be kept before starting"
    };
    if let Err(error) = store.set_review_session_reason(review_id, reason) {
        tracing::warn!(%error, review_id, "could not record why no reviewer started");
    }
    tokio::spawn({
        let store = store.clone();
        let manager = manager.clone();
        let review_id = review_id.to_string();
        let head_sha = head_sha.to_string();
        let candidate_id = candidate.id.clone();
        async move {
            let unkept = unkept(retained.await);
            let starts = manager.review_starts().clone();
            // Waiting out another start, which may start the very reviewer
            // this would.
            let waited_out = claimed.is_none();
            let mut claimed = claimed;
            loop {
                // Resubmitted with another commit, or decided, meanwhile:
                // the start for that commit is that submission's. So is the
                // start for this commit submitted again with checks of its
                // own still running: its reviewer waits on those.
                if !store
                    .review_undecided_at(&review_id, &head_sha)
                    .unwrap_or(false)
                    || manager.external_checks().running(&review_id, &candidate_id)
                {
                    return;
                }
                let released = starts.released.notified();
                tokio::pin!(released);
                // Woken by a release from here on, even one before the claim
                // below is tried.
                released.as_mut().enable();
                let Some(start) = claimed
                    .take()
                    .or_else(|| starts.claim_at(&review_id, Some(&head_sha)))
                else {
                    // Held by another start, which lets go once it has
                    // started its reviewer or seen this commit replace its
                    // own. The bound is only a backstop.
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), released).await;
                    continue;
                };
                if let Some(reviewer) = waited_out
                    .then(|| reviewer_started_on_latest(&store, &review_id))
                    .flatten()
                {
                    // Said as started, not as this start still waiting.
                    if let Err(error) = store.set_review_session(&review_id, &reviewer) {
                        tracing::warn!(%error, review_id, "could not record the reviewer");
                    }
                    return;
                }
                spawn_claimed_review_session(
                    &store,
                    &manager,
                    start,
                    unkept,
                    StartedBy::Submission,
                )
                .await;
                return;
            }
        }
    });
    json!({ "state": "starting", "reason": reason })
}

/// What a submission's result says of its reviewer, from what
/// [`spawn_review_session_when_kept`] returned: whether one is reading it,
/// and the trigger that decided.
fn reviewer_says(reviewer: &Value, channel: &str) -> String {
    let source = reviewer["source"].as_str().unwrap_or("session");
    let trigger = reviewer["trigger"].as_str().unwrap_or("auto");
    let binding = format!("phases.review.trigger.{source} is {trigger} on {channel}");
    match (reviewer["state"].as_str(), reviewer["model"].as_str()) {
        (Some("started" | "starting"), model) => format!(
            "An AI reviewer ({}) is reading this; its verdict is advisory and will appear in \
             review_status. Only the operator's decision publishes. It started because \
             {binding}.",
            model.unwrap_or("its review model")
        ),
        (_, None) => format!(
            "No AI reviewer runs: {channel} binds no review model (phases.review.model), so \
             none runs unless the operator binds one and starts it."
        ),
        _ if reviewer["reason"] == MANUAL_REVIEW => format!(
            "No AI reviewer runs unless the operator starts one from the review page: \
             {binding}."
        ),
        _ => format!(
            "No AI reviewer started: {}.",
            reviewer["reason"]
                .as_str()
                .unwrap_or("the node did not say why")
        ),
    }
}

/// What became of the reviewer of a review's latest revision, for
/// `review_status` and the review page: `starting`, `running`, `done` (its
/// verdict is in), `failed` (it ended without one), or `none`; with the
/// model, and the verdict once there is one. A verdict on an earlier
/// revision is not this revision's, and is named only by its revision.
pub(crate) fn reviewer_view(store: &Store, manager: &Manager, r: &ReviewRow) -> Value {
    let binding = ReviewerBinding::for_channel(manager, &r.channel);
    let source = Source::of(r);
    // Ids only: a revision's number is its place among them.
    let revisions = store.review_revision_ids(&r.id).unwrap_or_default();
    let number = |id: Option<&str>| {
        id.and_then(|id| revisions.iter().position(|rev| rev == id))
            .map(|i| i + 1)
    };
    let current = revisions.last().cloned();
    let placed = placed_verdict(store, r, &revisions);
    let verdict_current = placed.as_ref().is_some_and(|p| p.current);
    let verdict_revision = placed.as_ref().and_then(|p| p.revision.clone());
    let verdict = placed.map(|p| p.verdict);
    let session = r
        .review_session_id
        .as_deref()
        .and_then(|id| store.get_session(id).ok().flatten());
    let session_current = session.as_ref().is_some_and(|session| {
        current.is_some() && store.review_session_revision_id(session).ok().flatten() == current
    });
    let ended = |session: &crate::store::SessionRow| {
        crate::session::state::SessionState::TERMINAL.contains(&session.state.as_str())
    };
    let mut reason = None;
    let state = match &session {
        _ if manager.review_starts().pending(&r.id) => "starting",
        // Done once its verdict is in, while its session winds down.
        Some(session)
            if session_current
                && verdict
                    .as_ref()
                    .is_some_and(|v| v["session_id"] == session.id) =>
        {
            "done"
        }
        Some(session) if session_current && !ended(session) => {
            if session.state == "starting" {
                "starting"
            } else {
                "running"
            }
        }
        Some(session) if session_current => {
            reason = Some(match session.state.as_str() {
                "killed_budget" => {
                    "the reviewer used its token budget before giving a verdict".to_string()
                }
                _ => match &session.last_error {
                    Some(error) => format!("the reviewer ended without a verdict: {error}"),
                    None => "the reviewer ended without a verdict".to_string(),
                },
            });
            "failed"
        }
        _ if verdict_current => "done",
        _ => {
            reason = store.review_session_reason(&r.id).ok().flatten();
            "none"
        }
    };
    let mut view = json!({
        "state": state,
        "model": session
            .as_ref()
            .filter(|_| session_current)
            .map(|session| session.model.clone())
            .or_else(|| binding.model.clone()),
        // As it applied to this revision; the binding's now for one
        // submitted before that was recorded.
        "trigger": current
            .as_deref()
            .and_then(|id| store.review_revision_trigger(id).ok().flatten())
            .unwrap_or_else(|| binding.trigger(source).as_str().to_string()),
        "source": source.as_str(),
        "bound": binding.model.is_some(),
        "reason": reason,
        "revision": number(current.as_deref()),
    });
    if let Some(session) = session.as_ref().filter(|_| session_current) {
        view["session_id"] = json!(session.id);
        view["session_state"] = json!(session.state);
        view["started_ms"] = json!(session.created_ms);
        view["started_by"] = json!(store.review_session_started_by(&session.id).ok().flatten());
    }
    if let Some(verdict) = &verdict {
        view["verdict_revision"] = json!(number(verdict_revision.as_deref()));
        if verdict_current {
            view["verdict"] = verdict["verdict"].clone();
            view["summary"] = verdict["summary"].clone();
            view["findings"] = verdict["findings"].clone();
            view["verdict_at_ms"] = verdict["at_ms"].clone();
        }
    }
    view
}

/// The revision a reviewer's verdict is about: recorded with it, or, for
/// one recorded before that was, the revision its session started from.
fn verdict_revision(store: &Store, verdict: &Value) -> Option<String> {
    verdict["revision_id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            let session = store
                .get_session(verdict["session_id"].as_str()?)
                .ok()
                .flatten()?;
            store.review_session_revision_id(&session).ok().flatten()
        })
}

/// A review's AI verdict, the revision it is on, and whether that is the
/// latest of `revisions` (the review's revision ids, oldest first). The one
/// place a verdict is judged current, for the page and `review_status` alike.
struct PlacedVerdict {
    verdict: Value,
    revision: Option<String>,
    current: bool,
}

fn placed_verdict(store: &Store, r: &ReviewRow, revisions: &[String]) -> Option<PlacedVerdict> {
    let verdict = serde_json::from_str::<Value>(r.ai_verdict_json.as_deref()?).ok()?;
    let revision = verdict_revision(store, &verdict);
    let current = revision.is_some() && revision.as_ref() == revisions.last();
    Some(PlacedVerdict {
        verdict,
        revision,
        current,
    })
}

/// A reviewer's verdict on a review's latest revision, if one is in.
pub(crate) fn current_verdict(store: &Store, r: &ReviewRow) -> Option<Value> {
    r.ai_verdict_json.as_ref()?;
    let revisions = store.review_revision_ids(&r.id).ok()?;
    placed_verdict(store, r, &revisions)
        .filter(|placed| placed.current)
        .map(|placed| placed.verdict)
}

/// What `review_status` says to a submitter still waiting once a reviewer's
/// verdict on its latest revision is in: the verdict is advisory, and the
/// operator's decision is the one that comes back.
pub(crate) fn reviewer_verdict_says(verdict: &Value) -> String {
    format!(
        "The AI reviewer ({}) has given its verdict on this revision: {}. It is advisory; the \
         operator has not decided yet, and their decision is the one that governs. You may \
         start on its findings (`reviewer.findings`) now; the operator's decision still comes \
         back from review_status.",
        verdict["model"].as_str().unwrap_or("its review model"),
        verdict["verdict"].as_str().unwrap_or("given"),
    )
}

/// The review's latest reviewer, when it was started on the review's latest
/// revision: what a start that waited out another (one by hand, say, which
/// reads the latest revision when it runs) would otherwise start a second
/// time.
fn reviewer_started_on_latest(store: &Store, review_id: &str) -> Option<String> {
    let latest = store.latest_review_revision(review_id).ok().flatten()?;
    let (session_id, revision_id) = store.review_sessions_of(review_id).ok()?.pop()?;
    (revision_id == latest.id).then_some(session_id)
}

/// Why a reviewer has not started while the node keeps the candidate's files
/// and runs its checks; what the review page reads as a reviewer to come.
pub const WAITING_FOR_CHECKS: &str = "waiting for the candidate's files to be kept, and any \
                                      checks the node runs on them, before starting";

/// What every reason a start is waiting on begins with.
const WAITING: &str = "waiting for the candidate's files to be kept";

/// After a restart: the checks of external candidates the node was running
/// when it stopped never finish, so an undecided review still saying they
/// are running is told they were interrupted, as their `check_run` rows are
/// (`Store::reconcile_interrupted_runs`). A reviewer that was waiting, on
/// those checks or on files whose keeping the restart also ended, is started
/// now, or says why it cannot be; never on a review decided meanwhile. Only
/// this node's own reviews: a mirrored one says what its owner is doing.
pub async fn reconcile_interrupted_checks(store: &Arc<Store>, manager: &Manager) {
    let interrupted = unrun_checks(
        "the node restarted while it ran them; resubmit the candidate to run them again",
    );
    let mut reviews = match store.interrupt_running_checks(manager.node_id(), &interrupted) {
        Ok(reviews) => reviews,
        Err(error) => {
            tracing::error!(%error, "could not mark interrupted external checks");
            Vec::new()
        }
    };
    if !reviews.is_empty() {
        tracing::warn!(
            reviews = reviews.len(),
            "marked external checks a restart interrupted"
        );
    }
    // A reviewer whose wait the restart ended, its checks done or not: the
    // task that would have started it is gone.
    match store.undecided_reviews_waiting(manager.node_id(), WAITING) {
        Ok(waiting) => reviews.extend(waiting),
        Err(error) => tracing::error!(%error, "could not find reviewers a restart left waiting"),
    }
    reviews.sort();
    reviews.dedup();
    let mut started = false;
    for review_id in &reviews {
        let Some(review) = store.get_review(review_id).ok().flatten() else {
            continue;
        };
        let waiting = review_session_waits(store, review_id);
        // A review decided meanwhile wants no reviewer.
        let undecided = store
            .review_undecided_at(review_id, &review.head_sha)
            .unwrap_or(false);
        if waiting && undecided {
            spawn_review_session(store, manager, review_id, StartedBy::Submission).await;
            started = true;
        }
    }
    if started || !reviews.is_empty() {
        manager.publish_queue().await;
    }
}

/// Whether a review's reviewer is waiting on its files or checks to start.
fn review_session_waits(store: &Store, review_id: &str) -> bool {
    store
        .review_session_reason(review_id)
        .ok()
        .flatten()
        .is_some_and(|reason| reason.starts_with(WAITING))
}

/// The model a channel binds for review sessions, if any.
fn review_model(manager: &Manager, channel: &str) -> Option<String> {
    ReviewerBinding::for_channel(manager, channel).model
}

/// Why no reviewer started on a submission the channel reviews by hand.
pub const MANUAL_REVIEW: &str = "manual review: start a reviewer from the review page";

/// Whether a submission starts a reviewer on its own (`auto`), or only the
/// operator starts one from the review page (`manual`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Auto,
    Manual,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Auto => "auto",
            Trigger::Manual => "manual",
        }
    }

    fn parse(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "auto" => Some(Trigger::Auto),
            "manual" => Some(Trigger::Manual),
            _ => None,
        }
    }
}

/// Where a submission came from, which the trigger is set for separately:
/// a session this node runs, whose checks ran and whose work was shown, or
/// a harness the operator runs, which sends only its diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Session,
    External,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Session => "session",
            Source::External => "external",
        }
    }

    /// The source of a recorded review: an external harness has no session.
    pub fn of(review: &ReviewRow) -> Self {
        if review.session_id.is_some() {
            Source::Session
        } else {
            Source::External
        }
    }
}

/// How a review session came to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartedBy {
    /// On a submission, under an `auto` trigger.
    Submission,
    /// By the operator, from the review page.
    Operator,
}

impl StartedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            StartedBy::Submission => "submission",
            StartedBy::Operator => "operator",
        }
    }
}

/// How a reviewer starts for a source when the channel's
/// `phases.review.trigger` says nothing for it. The one place that default
/// is decided: an external harness's submission carries no checks or shown
/// work, so a reviewer on its own would have only the diff to go on, unless
/// the node runs the repository's checks on it (`[external] run_checks`),
/// which the reviewer then waits for and reads.
pub fn default_trigger(source: Source, external_checks: bool) -> Trigger {
    match source {
        Source::Session => Trigger::Auto,
        Source::External if external_checks => Trigger::Auto,
        Source::External => Trigger::Manual,
    }
}

/// A channel's reviewer: the model it binds (`phases.review.model`) and its
/// trigger for each source (`phases.review.trigger.session` and
/// `.external`). A bare `phases.review.trigger = "manual"` sets both. Without
/// a model no reviewer starts either way. What a binding leaves out is
/// [`default_trigger`]'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerBinding {
    pub model: Option<String>,
    pub session: Trigger,
    pub external: Trigger,
}

impl ReviewerBinding {
    /// A channel's binding on this node, whose `[external] run_checks` sets
    /// the default for external submissions.
    pub fn for_channel(manager: &Manager, channel: &str) -> Self {
        Self::of(
            &manager.bindings(channel),
            manager.cfg().external.run_checks,
        )
    }

    /// `bindings` read with the defaults for a node that does, or does not,
    /// run checks on external submissions (`external_checks`).
    pub fn of(bindings: &Value, external_checks: bool) -> Self {
        let review = &bindings["phases"]["review"];
        let model = review["model"]
            .as_str()
            .filter(|m| !m.trim().is_empty())
            .map(str::to_string);
        let trigger = &review["trigger"];
        let of = |source: Source| {
            Trigger::parse(trigger)
                .or_else(|| Trigger::parse(&trigger[source.as_str()]))
                .unwrap_or_else(|| default_trigger(source, external_checks))
        };
        ReviewerBinding {
            model,
            session: of(Source::Session),
            external: of(Source::External),
        }
    }

    pub fn trigger(&self, source: Source) -> Trigger {
        match source {
            Source::Session => self.session,
            Source::External => self.external,
        }
    }

    /// The triggers in effect, defaults included, as the settings screen
    /// shows them.
    pub fn view(&self) -> Value {
        json!({
            "session": self.session.as_str(),
            "external": self.external.as_str(),
            "model": self.model,
        })
    }
}

/// Start a review session from the review's latest candidate: its session
/// id, or whether none was due (`none`) or one could not start (`failed`),
/// and why.
async fn start_review_session(
    store: &Arc<Store>,
    manager: &Manager,
    review_id: &str,
    at: Option<&str>,
    unkept: Option<&str>,
    started_by: StartedBy,
) -> Result<String, (&'static str, String)> {
    let none = |reason: &str| ("none", reason.to_string());
    let failed = |error: &dyn std::fmt::Display| ("failed", error.to_string());
    let r = store
        .get_review(review_id)
        .map_err(|e| failed(&e))?
        .ok_or_else(|| none("review not found"))?;
    let Some(model) = review_model(manager, &r.channel) else {
        return Err(none(
            "no review model bound on this channel (phases.review.model)",
        ));
    };
    let revision = store
        .latest_review_revision(review_id)
        .map_err(|e| failed(&e))?
        .ok_or_else(|| none("this review has no recorded revision to start a reviewer from"))?;
    if at.is_some_and(|head| head != revision.head_sha) {
        return Err(none(MOVED));
    }
    let candidate = store
        .candidate(&revision.candidate_id)
        .map_err(|e| failed(&e))?
        .ok_or_else(|| none("the candidate this review is of is gone"))?;
    let materialized = serde_json::from_str::<Value>(&candidate.capture_json)
        .ok()
        .and_then(|capture| capture["materialized"].as_bool())
        .unwrap_or(false);
    if !materialized {
        return Err(none(&match unkept {
            Some(why) => format!(
                "the candidate's files were not kept ({why}), so there is no tree to start a \
                 reviewer from"
            ),
            None => "the candidate's files were not kept, so there is no tree to start a \
                     reviewer from"
                .to_string(),
        }));
    }
    let mut files = store
        .candidate_files(&candidate.id)
        .map_err(|e| failed(&e))?;
    // The whole diff beside the tree, where the instructions point for the
    // part they leave out.
    files.retain(|file| file.path != ".tracon" && file.path != REVIEW_DIFF);
    files.push(crate::store::CandidateFile {
        path: REVIEW_DIFF.into(),
        mode: 0o100644,
        content: revision.diff.clone().into_bytes(),
    });
    // Writable at its top only, for the repository a session needs.
    let snapshot = candidate_tree(&candidate, files, true)
        .await
        .map_err(|e| failed(&e))?;
    let workspace_id = format!("review-{review_id}");
    let short = &review_id[review_id.len().saturating_sub(12)..];
    let branch = format!("review/{short}");
    let git = manager.cfg().live().publish.git.clone();
    let seeded = match crate::workspace::init_empty_git(&git, &snapshot.root, &branch).await {
        Ok(()) => {
            crate::workspace::from_snapshot(
                manager.backend().as_ref(),
                &workspace_id,
                &snapshot.root,
            )
            .await
        }
        Err(error) => Err(error),
    };
    tokio::task::spawn_blocking(move || drop(snapshot));
    seeded.map_err(|e| failed(&e))?;
    // No session imported this workspace, so nothing leads from it back to a
    // repository: the review's is recorded on it before the session starts,
    // and the session gets that repository's image and environment from it.
    let repo = crate::environment::review_repo(store, &r).map_err(|e| failed(&e))?;
    store
        .record_workspace_origin(
            &workspace_id,
            repo.as_deref()
                .map(|repo| repo.to_string_lossy())
                .as_deref(),
        )
        .map_err(|e| failed(&e))?;
    let bindings = manager.bindings(&r.channel);
    let spec = crate::session::NewSession {
        channel: r.channel.clone(),
        repo_path: String::new(),
        branch: Some(branch),
        base: None,
        // The item this revision was pinned to, not whatever the submitting
        // session holds now; a harness the operator runs has no session.
        work_item_id: revision.requirements_work_item_id.clone(),
        workspace_id: Some(workspace_id),
        parent_session: None,
        continued_from: None,
        harness: None,
        on_exhaustion: None,
        told_reviews: Vec::new(),
        model,
        budget_tokens: bindings["phases"]["review"]["budget_tokens"].as_i64(),
        initial_prompt: None,
        node_id: None,
        phase: crate::session::Phase::Review,
        review_id: Some(review_id.to_string()),
        base_sha: Some(revision.head_sha.clone()),
    };
    let row = manager.create(spec).await.map_err(|e| failed(&e))?;
    store
        .set_review_session(review_id, &row.id)
        .map_err(|e| failed(&e))?;
    // What it shows and finds stays about the revision it was started from,
    // even after a resubmission starts another reviewer beside it.
    if let Err(error) =
        store.review_session_start_record(&row.id, review_id, &revision.id, started_by.as_str())
    {
        tracing::warn!(%error, review_id, "could not record the revision a reviewer started from");
    }
    manager.publish_queue().await;
    Ok(row.id)
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
            "only a review session gives a verdict; an execute session calls submit_review".into(),
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
    // A reviewer started before verdicts said what they rest on was given
    // no `basis` to send: its verdict stands, as resting on the diff.
    let basis = match args.get("basis") {
        None | Some(Value::Null) => "diff_only".to_string(),
        Some(_) => str_arg(args, "basis")?,
    };
    if !BASES.contains(&basis.as_str()) {
        return Err(format!(
            "{basis:?} is not a basis; say what the verdict rests on: {}",
            BASES.join(", ")
        ));
    }
    let summary = str_arg(args, "summary")?;
    let (findings, adjusted) = findings_arg(args);
    // The revision it read, so a resubmission does not wear it. Not finding
    // it is no reason to lose the verdict: one without it is placed by its
    // session, as a verdict recorded before this was.
    let revision_id = store
        .review_session_revision_id(&session)
        .unwrap_or_else(|error| {
            tracing::warn!(%error, %review_id, "could not read the revision a verdict is on");
            None
        });
    let v = json!({
        "verdict": verdict, "basis": basis, "summary": summary, "findings": findings,
        "model": session.model, "session_id": session_id, "at_ms": now_ms(),
        "revision_id": revision_id,
    });
    let state = match store
        .set_ai_verdict(&review_id, &v.to_string(), revision_id.as_deref())
        .map_err(|e| e.to_string())?
    {
        VerdictRecorded::Recorded => None,
        VerdictRecorded::Superseded => {
            // A later revision's reviewer has already given its verdict;
            // this one is kept as an event, not put over that.
            manager.record_event(
                session_id,
                ek::REVIEW_VERDICT,
                json!({
                    "review_id": review_id, "verdict": verdict, "summary": summary,
                    "revision_id": revision_id, "superseded": true,
                }),
            );
            manager.phase_done(session_id).await;
            return Ok(json!({
                "review_id": review_id,
                "recorded": false,
                "superseded": true,
                "message": "The review was resubmitted, and the reviewer of the later revision \
                            has already given its verdict; yours, on an earlier revision, is \
                            kept in this session's events and does not replace it.",
            }));
        }
        VerdictRecorded::Decided(state) => Some(Some(state)),
        VerdictRecorded::Gone => Some(None),
    };
    if let Some(state) = state {
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
        // Whole, so a reviewer of a later revision is told what this one
        // found (`corpus::orientation`), not only which way it went.
        json!({
            "review_id": review_id, "verdict": verdict, "basis": basis,
            "summary": summary, "findings": findings,
        }),
    );
    manager.publish_queue().await;
    // A submitter held waiting on this review hears the verdict now, rather
    // than only once the operator decides.
    if let Some(submitter) = store
        .get_review(&review_id)
        .ok()
        .flatten()
        .and_then(|review| review.session_id)
    {
        manager.wait_moved(&submitter).await;
    }
    manager.phase_done(session_id).await;
    let mut recorded = json!({ "review_id": review_id, "recorded": true });
    if !adjusted.is_empty() {
        recorded["findings_adjusted"] = json!(adjusted);
    }
    Ok(recorded)
}

/// What `report_status` answers for a narrative report the operator has
/// decided, or `None` while it waits on them.
pub(crate) fn decided_report_view(report: &ReviewRow) -> Option<Value> {
    match report.state.as_str() {
        crate::store::reports::ACKNOWLEDGED => Some(json!({
            "report_id": report.id,
            "state": "acknowledged",
            "note": report.verdict_reason,
            "message": "The operator acknowledged this report. This records receipt only; no code was published.",
        })),
        "revising" => Some(json!({
            "report_id": report.id,
            "state": "changes_requested",
            "notes": report.verdict_reason,
            "message": "The operator requested changes. Revise the narrative and call submit_report with this report_id; do not submit code for publication.",
        })),
        _ => None,
    }
}

/// Whether another of `session`'s reviews already pushes to `branch`. The
/// first review renames the session's placeholder branch to its own; a later
/// new review is a change of its own, not an update of that one's.
fn pushed_by_another(
    store: &Store,
    session: Option<&crate::store::SessionRow>,
    branch: &str,
) -> bool {
    session.is_some_and(|s| {
        store
            .reviews_of_session(&s.id)
            .unwrap_or_default()
            .iter()
            .any(|r| serde_json::from_str::<Target>(&r.target).is_ok_and(|t| t.branch == branch))
    })
}

/// What `review_status` answers when the operator approved `r` but
/// publishing it failed, and what a waiting session is told of it.
pub(crate) fn publish_failed_view(
    r: &ReviewRow,
    failed: &crate::store::PublicationRow,
    says: &str,
    brought_back: &str,
) -> Value {
    let error = failed.note.clone().unwrap_or_default();
    json!({
        "review_id": r.id,
        "state": "publish_failed",
        "still_waiting": true,
        "publication": {
            "error": error,
            "attempts": failed.attempts,
            "branch": failed.branch,
            "base": failed.base,
        },
        "message": format!(
            "{says}The operator approved this review, but publishing it failed: \
             {error}. It is back with the operator, who can retry the same \
             publication or reject it. If the failure names something to change, \
             change it and resubmit with the same review_id. Call review_status \
             again with the same review_id to keep waiting.{brought_back}"
        ),
    })
}

/// What `review_status` answers for a review the operator has decided, or
/// `None` while it waits on them.
pub(crate) fn decided_review_view(r: &ReviewRow) -> Option<Value> {
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
            Some(json!({
                "review_id": r.id,
                "state": "changes_requested",
                "notes": r.verdict_reason,
                "patch": r.revision_patch,
                "message": message,
            }))
        }
        "approved" | "rejected" => Some(json!({
            "review_id": r.id,
            "state": r.state,
            // The approved text, which may not be what was submitted.
            "title": r.approved_title(),
            "body": r.approved_body(),
            "reason": r.verdict_reason,
            "published": r.publish_result,
        })),
        _ => None,
    }
}

/// What `review_status` answers for a submission whose checks failed before
/// a review was recorded — what `submit_review` would have refused with, had
/// it waited — unless the review it would have revised was decided since.
/// `None` while the checks run, or when they did not fail that way.
pub(crate) fn submit_failed_view(
    store: &Store,
    entry: &crate::mcp::checking::Entry,
) -> Option<Value> {
    let Some(Err(reason)) = entry.outcome() else {
        return None;
    };
    if entry.recorded() {
        return None;
    }
    let decided = store
        .get_review(&entry.handle)
        .ok()
        .flatten()
        .is_some_and(|r| matches!(r.state.as_str(), "approved" | "rejected"));
    (!decided).then(|| {
        json!({
            "review_id": entry.handle,
            "state": "submit_failed",
            "candidate_id": entry.candidate_id,
            "checks": entry.commands,
            "message": reason,
        })
    })
}

/// The calling session has now been told what became of this review: the
/// node need not bring it back as a prompt.
fn told_review(manager: &Manager, ctx: &CallContext, review: &ReviewRow) {
    if let Some(session_id) = ctx.session_id() {
        manager
            .waits()
            .tell(session_id, &crate::session::waits::review_key(review));
    }
}

/// Poll a narrative report without implying that it could publish anything.
async fn report_status(
    store: &Arc<Store>,
    manager: &Manager,
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
        if let Some(view) = decided_report_view(&report) {
            told_review(manager, ctx, &report);
            return Ok(view);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(json!({
                "report_id": report.id,
                "state": report.state,
                "still_waiting": true,
                "waited_secs": wait,
                "message": format!(
                    "Still waiting on an operator acknowledgement. The wait is capped at \
                     {MAX_WAIT_SECS} seconds; call report_status again with the same \
                     report_id.{}",
                    crate::session::waits::brought_back(ctx.session_id().is_some())
                ),
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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
    let began = tokio::time::Instant::now();
    let deadline = began + std::time::Duration::from_secs(wait);

    // Checks the node is running for this handle come first: until they
    // settle, the review either does not exist yet or is still the revision
    // they are meant to replace.
    if let Some(entry) = manager
        .checking()
        .get(&id)
        .filter(|entry| Some(entry.session_id.as_str()) == ctx.session_id())
    {
        if entry.wait(deadline).await.is_none() {
            let mut response = checking(&entry);
            response["still_waiting"] = json!(true);
            response["waited_secs"] = json!(wait);
            response["message"] = json!(format!(
                "Still running the required checks ({}s so far); nothing reaches the \
                 operator until they pass. The wait is capped at {MAX_WAIT_SECS} seconds; \
                 call review_status again with the same review_id.{}",
                entry.running_secs(),
                crate::session::waits::brought_back(true)
            ));
            return Ok(response);
        }
        if let Some(view) = submit_failed_view(store, &entry) {
            manager.waits().tell(
                &entry.session_id,
                &crate::session::waits::checks_key(&entry),
            );
            return Ok(view);
        }
    }

    // A verdict this caller has not been told of ends the wait early, so the
    // submitter can start on it, whether it landed during this call or
    // between this one and the last. A harness outside the node has no
    // session to be told as, so it is told as the review's external caller.
    let caller = match ctx.session_id() {
        Some(session_id) => session_id.to_string(),
        None => external_caller(&id),
    };
    // The verdict and any failed publication as of the review's last change,
    // read again only when it changes rather than on every poll.
    let mut read: Option<(i64, Option<Value>, Option<crate::store::PublicationRow>)> = None;
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
        if let Some(view) = decided_review_view(&r) {
            told_review(manager, ctx, &r);
            review_decided(manager, &r.id);
            return Ok(view);
        }
        if read
            .as_ref()
            .is_none_or(|(updated, ..)| *updated != r.updated_ms)
        {
            read = Some((
                r.updated_ms,
                current_verdict(store, &r),
                store.failed_publication(&r).map_err(|e| e.to_string())?,
            ));
        }
        let (verdict, failed) = read
            .as_ref()
            .map(|(_, verdict, failed)| (verdict.clone(), failed.clone()))
            .unwrap_or_default();
        let untold = verdict.as_ref().is_some_and(|verdict| {
            !manager.waits().was_told(
                &caller,
                &crate::session::waits::reviewer_key(&r.id, verdict),
            )
        });
        // An approval that failed to publish put the review back with the
        // operator. The submitter hears of it once per attempt, the way it
        // hears of a verdict, so it can fix what the failure names.
        let untold_failure = failed.as_ref().is_some_and(|row| {
            !manager
                .waits()
                .was_told(&caller, &crate::session::waits::publication_key(row))
        });
        let now = tokio::time::Instant::now();
        if untold || untold_failure || now >= deadline {
            let says = match &verdict {
                Some(verdict) => {
                    manager.waits().tell(
                        &caller,
                        &crate::session::waits::reviewer_key(&r.id, verdict),
                    );
                    format!("{} ", reviewer_verdict_says(verdict))
                }
                None => String::new(),
            };
            let waited = if now >= deadline {
                wait
            } else {
                (now - began).as_secs()
            };
            if let Some(failed) = &failed {
                manager
                    .waits()
                    .tell(&caller, &crate::session::waits::publication_key(failed));
                let mut view = publish_failed_view(
                    &r,
                    failed,
                    &says,
                    crate::session::waits::brought_back(ctx.session_id().is_some()),
                );
                view["waited_secs"] = json!(waited);
                view["reviewer"] = reviewer_view(store, manager, &r);
                return Ok(view);
            }
            return Ok(json!({
                "review_id": r.id,
                "state": r.state,
                "still_waiting": true,
                // What it waited, which a verdict can cut short of the wait asked.
                "waited_secs": waited,
                "reviewer": reviewer_view(store, manager, &r),
                "message": format!(
                    "{says}Still waiting on a human; the review is {}. The wait is capped at \
                     {MAX_WAIT_SECS} seconds so this call returns before your MCP client \
                     times out — asking for longer will not help. Call review_status again \
                     with the same review_id to keep waiting.{}",
                    r.state,
                    crate::session::waits::brought_back(ctx.session_id().is_some())
                ),
            }));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// `review_verdict`'s findings, as its schema gives them: each an object
/// with a non-empty `note`, and a `path`, a `line` and a `severity` only of
/// the kinds named. What the review page and the submitter read is what
/// this lets through. One malformed finding does not cost the verdict: a
/// part of it that is not of its kind is left off, a finding with nothing
/// saying what was found is dropped, and what was done is said back.
fn findings_arg(args: &Value) -> (Value, Vec<String>) {
    let mut adjusted = Vec::new();
    let list = match args.get("findings") {
        None | Some(Value::Null) => return (json!([]), adjusted),
        Some(Value::Array(list)) => list.as_slice(),
        Some(_) => {
            adjusted.push("`findings` is not a list, so none were recorded".to_string());
            return (json!([]), adjusted);
        }
    };
    let mut kept = Vec::new();
    for (i, finding) in list.iter().enumerate() {
        let n = i + 1;
        let mut finding = match finding {
            Value::Object(finding) => finding.clone(),
            // A bare string says what was found and nothing else.
            Value::String(note) => {
                let mut finding = serde_json::Map::new();
                finding.insert("note".into(), json!(note));
                finding
            }
            _ => {
                adjusted.push(format!("finding {n} is not an object, so it was dropped"));
                continue;
            }
        };
        if !finding
            .get("note")
            .and_then(Value::as_str)
            .is_some_and(|note| !note.trim().is_empty())
        {
            adjusted.push(format!(
                "finding {n} has no `note` saying what was found, so it was dropped"
            ));
            continue;
        }
        let mut leave_off = |finding: &mut serde_json::Map<String, Value>, key: &str, why: &str| {
            finding.remove(key);
            adjusted.push(format!("finding {n}'s `{key}` {why}, so it was left off"));
        };
        if finding
            .get("path")
            .is_some_and(|p| !(p.is_null() || p.is_string()))
        {
            leave_off(&mut finding, "path", "is not a string");
        }
        if finding
            .get("line")
            .is_some_and(|l| !(l.is_null() || l.as_u64().is_some_and(|l| l > 0)))
        {
            leave_off(&mut finding, "line", "is not a line number");
        }
        if finding.get("severity").is_some_and(|s| {
            !(s.is_null() || matches!(s.as_str(), Some("blocking" | "should" | "nit")))
        }) {
            leave_off(
                &mut finding,
                "severity",
                "is not one of blocking, should or nit",
            );
        }
        kept.push(Value::Object(finding));
    }
    (Value::Array(kept), adjusted)
}

/// Who a harness outside the node is told a review's AI verdict as: it has
/// no session, and only the review's submitter may ask about it.
fn external_caller(review_id: &str) -> String {
    format!("external:{review_id}")
}

/// A review was decided: what the node kept for its reviewer's start and
/// its external caller's wait is no longer needed.
pub(crate) fn review_decided(manager: &Manager, review_id: &str) {
    manager.review_starts().unkeep(review_id);
    manager.waits().forget(&external_caller(review_id));
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
    fn a_reused_failure_says_resubmitting_the_same_commit_returns_it_again() {
        let mut failed = review::checks::CheckResult {
            command: "uv run --no-sync ruff check .".into(),
            ok: false,
            exit: Some(2),
            outcome: "failed".into(),
            tail: "error: Failed to initialize cache".into(),
            failures: String::new(),
            ms: 0,
            reused_from: None,
            missing_tool: None,
        };
        let fresh = failed_check_refusal(&failed);
        assert!(
            fresh.contains("(exit 2). Fix it and submit again."),
            "{fresh}"
        );
        assert!(!fresh.contains("rerun_checks"), "{fresh}");

        failed.outcome = "reused".into();
        failed.reused_from = Some("run-1".into());
        let reused = failed_check_refusal(&failed);
        assert!(reused.starts_with("check reused: `uv run"), "{reused}");
        assert!(
            reused.contains(
                "submitting the same commit again returns it again unless you pass \
                 `rerun_checks: true`"
            ),
            "{reused}"
        );
        assert!(
            reused.contains("resubmit with `rerun_checks: true`"),
            "{reused}"
        );
        assert!(!reused.contains("Fix it and submit again"), "{reused}");
        assert!(reused.contains("Failed to initialize cache"), "{reused}");
    }

    /// The node failing to run an external candidate's checks lands on the
    /// review as that, on the commit they were for, so the card and the
    /// reviewer can tell it from there being no checks.
    /// A review from a harness the operator runs, on node `n`, at `head`.
    fn review_at(head: &str) -> ReviewRow {
        ReviewRow {
            id: "r1".into(),
            session_id: None,
            node_id: "n".into(),
            channel: "work".into(),
            kind: "pr".into(),
            title: "t".into(),
            body: "b".into(),
            edited_title: None,
            edited_body: None,
            provider: "github".into(),
            target: "{}".into(),
            diff: "d".into(),
            files: "[]".into(),
            head_sha: head.into(),
            base_ref: "main".into(),
            added: 1,
            removed: 0,
            state: "new".into(),
            verdict_reason: None,
            publish_result: None,
            claimed_ms: None,
            created_ms: 1,
            created_mono_ms: 0,
            resolved_mono_ms: None,
            updated_ms: 1,
            checks_json: None,
            review_session_id: None,
            ai_verdict_json: None,
            revision_patch: None,
            lane: None,
            publication_failed: None,
        }
    }

    /// A run of an external candidate's checks under way is attached to, and
    /// one that has ended is not: a submission after it starts its own, and
    /// nothing says checks are running that nothing is running.
    #[test]
    fn a_check_run_that_has_ended_is_not_attached_to() {
        let store = Store::open_in_memory().unwrap();
        store.insert_review(&review_at("sha1")).unwrap();
        let candidate = CandidateRow {
            id: "c1".into(),
            head_sha: "sha1".into(),
            tree_sha: None,
            channel: "work".into(),
            owner_session_id: None,
            source_kind: "git".into(),
            captured_ms: 1,
            capture_json: "{}".into(),
        };
        let checks = Arc::new(ExternalChecks::default());
        let running = checks
            .begin(&store, "r1", &candidate)
            .expect("the first run");
        assert_eq!(checks.in_flight(), 1);
        // Attached to while it runs, and said to be running again.
        assert!(checks.begin(&store, "r1", &candidate).is_none());
        assert_eq!(
            store
                .get_review("r1")
                .unwrap()
                .unwrap()
                .checks_json
                .as_deref(),
            Some(running_checks().as_str())
        );
        // Asked to run again: still under way, still attached to.
        assert!(running.settle(|| true));
        assert!(checks.begin(&store, "r1", &candidate).is_none());
        // Ended: the next submission starts its own run.
        assert!(!running.settle(|| false));
        assert_eq!(checks.in_flight(), 0);
        store.set_checks("r1", None).unwrap();
        let next = checks.begin(&store, "r1", &candidate);
        assert!(next.is_some());
        assert_eq!(store.get_review("r1").unwrap().unwrap().checks_json, None);
        drop(next);
        drop(running);
        assert_eq!(checks.in_flight(), 0);
    }

    /// A run that has settled no longer owns its key: dropping its guard
    /// leaves a later run of the same commit counted as running.
    #[test]
    fn a_settled_run_leaves_a_later_runs_key_alone() {
        let store = Store::open_in_memory().unwrap();
        store.insert_review(&review_at("sha1")).unwrap();
        let candidate = CandidateRow {
            id: "c1".into(),
            head_sha: "sha1".into(),
            tree_sha: None,
            channel: "work".into(),
            owner_session_id: None,
            source_kind: "git".into(),
            captured_ms: 1,
            capture_json: "{}".into(),
        };
        let checks = Arc::new(ExternalChecks::default());
        let a = checks.begin(&store, "r1", &candidate).expect("run A");
        assert!(!a.settle(|| false));
        let b = checks.begin(&store, "r1", &candidate).expect("run B");
        // A second settle of A, and its drop, are nothing to B.
        assert!(!a.settle(|| false));
        drop(a);
        assert!(checks.running("r1", "c1"));
        assert_eq!(checks.in_flight(), 1);
        drop(b);
        assert!(!checks.running("r1", "c1"));
    }

    /// After a restart a review still saying its checks are running is told
    /// they were interrupted; another node's review is its owner's to say.
    #[test]
    fn checks_a_restart_interrupted_are_found_on_this_nodes_reviews_only() {
        let store = Store::open_in_memory().unwrap();
        store.insert_review(&review_at("sha1")).unwrap();
        assert!(store
            .set_checks_at("r1", "sha1", &running_checks())
            .unwrap());
        let interrupted = unrun_checks("the node restarted");
        assert!(store
            .interrupt_running_checks("elsewhere", &interrupted)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.interrupt_running_checks("n", &interrupted).unwrap(),
            vec!["r1".to_string()]
        );
        assert_eq!(
            store
                .get_review("r1")
                .unwrap()
                .unwrap()
                .checks_json
                .as_deref(),
            Some(interrupted.as_str())
        );
        assert!(store
            .interrupt_running_checks("n", &interrupted)
            .unwrap()
            .is_empty());
    }

    /// A review decided while its checks still said they were running is
    /// left as it is, and is no reviewer's to wait for.
    #[test]
    fn a_decided_review_left_running_is_not_reconciled() {
        let store = Store::open_in_memory().unwrap();
        let mut decided = review_at("sha1");
        decided.id = "r2".into();
        decided.state = "approved".into();
        decided.checks_json = Some(running_checks());
        store.insert_review(&decided).unwrap();
        store
            .set_review_session_reason("r2", WAITING_FOR_CHECKS)
            .unwrap();
        store.insert_review(&review_at("sha1")).unwrap();
        store
            .set_review_session_reason("r1", WAITING_FOR_CHECKS)
            .unwrap();
        assert!(store
            .interrupt_running_checks("n", &unrun_checks("the node restarted"))
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .get_review("r2")
                .unwrap()
                .unwrap()
                .checks_json
                .as_deref(),
            Some(running_checks().as_str())
        );
        assert_eq!(
            store.undecided_reviews_waiting("n", WAITING).unwrap(),
            vec!["r1".to_string()]
        );
        assert!(store
            .undecided_reviews_waiting("elsewhere", WAITING)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn checks_the_node_could_not_run_are_recorded_as_that() {
        let store = Store::open_in_memory().unwrap();
        let review = review_at("sha2");
        store.insert_review(&review).unwrap();
        // For a commit the review has moved past: nothing lands.
        assert!(!record_unrun_checks(&store, "r1", "sha1", "the candidate is gone").unwrap());
        assert_eq!(store.get_review("r1").unwrap().unwrap().checks_json, None);
        assert!(record_unrun_checks(&store, "r1", "sha2", "no space left on device").unwrap());
        let checks: Vec<review::checks::CheckResult> = serde_json::from_str(
            store
                .get_review("r1")
                .unwrap()
                .unwrap()
                .checks_json
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].outcome, COULD_NOT_RUN);
        assert!(!checks[0].ok);
        assert!(
            checks[0].tail.contains("no space left on device"),
            "{checks:?}"
        );
    }

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

    /// The title and body become the forge's description by default, so the
    /// tool says who reads them: the repository's reviewers, not the operator,
    /// with notes for the operator sent to `show_work` instead.
    #[test]
    fn submit_says_who_reads_what_reaches_the_forge() {
        let def = definitions()
            .into_iter()
            .find(|d| d["name"] == SUBMIT)
            .expect("submit_review is defined");
        let description = def["description"].as_str().unwrap();
        assert!(
            !description.contains("summary for the operator"),
            "{description}"
        );
        assert!(
            description.contains("do not address the operator"),
            "{description}"
        );
        assert!(description.contains("do not name tracon"), "{description}");
        assert!(
            description.contains(crate::mcp::show::SHOW),
            "{description}"
        );
        let body = def["inputSchema"]["properties"]["body"]["description"]
            .as_str()
            .unwrap();
        assert!(body.contains("not the operator"), "{body}");
    }

    #[test]
    fn a_channel_reviews_its_own_sessions_on_their_own_and_external_harnesses_by_hand() {
        let bound = ReviewerBinding::of(&json!({"phases": {"review": {"model": "m/r"}}}), false);
        assert_eq!(bound.model.as_deref(), Some("m/r"));
        assert_eq!(bound.trigger(Source::Session), Trigger::Auto);
        assert_eq!(bound.trigger(Source::External), Trigger::Manual);
        // A fresh channel takes the same defaults, with no model to start one.
        let fresh = ReviewerBinding::of(&json!({}), false);
        assert_eq!(fresh.model, None);
        assert_eq!(
            fresh.view(),
            json!({"session": "auto", "external": "manual", "model": null})
        );
    }

    /// A node that runs the repository's checks on external submissions
    /// gives its reviewer something to go on, so external submissions start
    /// one on their own unless the channel says otherwise.
    #[test]
    fn a_node_that_checks_external_submissions_reviews_them_on_their_own() {
        let fresh = ReviewerBinding::of(&json!({}), true);
        assert_eq!(
            fresh.view(),
            json!({"session": "auto", "external": "auto", "model": null})
        );
        let manual = ReviewerBinding::of(
            &json!({"phases": {"review": {"trigger": {"external": "manual"}}}}),
            true,
        );
        assert_eq!(manual.external, Trigger::Manual);
        let both = ReviewerBinding::of(&json!({"phases": {"review": {"trigger": "manual"}}}), true);
        assert_eq!(
            (both.session, both.external),
            (Trigger::Manual, Trigger::Manual)
        );
        assert_eq!(default_trigger(Source::External, true), Trigger::Auto);
        assert_eq!(default_trigger(Source::External, false), Trigger::Manual);
    }

    #[test]
    fn a_trigger_is_set_per_source_or_for_both_at_once() {
        let each = ReviewerBinding::of(
            &json!({"phases": {"review": {
                "trigger": {"session": "manual", "external": "auto"}
            }}}),
            false,
        );
        assert_eq!(
            (each.session, each.external),
            (Trigger::Manual, Trigger::Auto)
        );
        let both =
            ReviewerBinding::of(&json!({"phases": {"review": {"trigger": "manual"}}}), false);
        assert_eq!(
            (both.session, both.external),
            (Trigger::Manual, Trigger::Manual)
        );
        // What it does not know is the default, not an error at submit.
        let odd = ReviewerBinding::of(
            &json!({"phases": {"review": {"trigger": {"session": "often"}}}}),
            false,
        );
        assert_eq!(odd.session, Trigger::Auto);
    }

    #[test]
    fn the_submitter_is_told_whether_a_reviewer_reads_its_change_and_why() {
        let running = reviewer_says(
            &json!({"state": "starting", "model": "m/r", "trigger": "auto", "source": "external"}),
            "work",
        );
        assert!(
            running.contains("An AI reviewer (m/r) is reading this"),
            "{running}"
        );
        assert!(running.contains("advisory"), "{running}");
        assert!(
            running.contains("phases.review.trigger.external is auto on work"),
            "{running}"
        );
        let manual = reviewer_says(
            &json!({"state": "none", "model": "m/r", "trigger": "manual", "source": "session",
                    "reason": MANUAL_REVIEW}),
            "work",
        );
        assert!(
            manual.contains("unless the operator starts one"),
            "{manual}"
        );
        let unbound = reviewer_says(
            &json!({"state": "none", "model": null, "trigger": "auto", "source": "session",
                    "reason": "no review model bound on this channel (phases.review.model)"}),
            "work",
        );
        assert!(unbound.contains("binds no review model"), "{unbound}");
        let failed = reviewer_says(
            &json!({"state": "failed", "model": "m/r", "trigger": "auto", "source": "session",
                    "reason": "the backend is down"}),
            "work",
        );
        assert!(
            failed.contains("No AI reviewer started: the backend is down"),
            "{failed}"
        );
    }

    /// A keep whose files arrive when `release` is sent, as `retain_candidate_files` would.
    fn keeping(
        starts: &Arc<ReviewStarts>,
        review_id: &str,
    ) -> tokio::sync::oneshot::Sender<Result<(), String>> {
        let (release, kept) = tokio::sync::oneshot::channel();
        starts.keep(
            review_id,
            "rev-1",
            tokio::spawn(async move { kept.await.unwrap_or(Ok(())).map(|()| None) }),
        );
        release
    }

    #[tokio::test]
    async fn an_earlier_submissions_keep_finishing_late_leaves_the_later_ones_in_place() {
        let starts = Arc::new(ReviewStarts::default());
        let first = keeping(&starts, "rv");
        let second = keeping(&starts, "rv");
        first.send(Ok(())).unwrap();
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // A start by hand still waits on the second, rather than finding none.
        let waiting = tokio::spawn({
            let starts = starts.clone();
            async move { starts.kept("rv", "rev-1").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !waiting.is_finished(),
            "the second keep's entry was removed"
        );
        second.send(Err("over the limit".into())).unwrap();
        assert_eq!(waiting.await.unwrap().as_deref(), Some("over the limit"));
    }

    #[tokio::test]
    async fn why_the_files_were_not_kept_is_there_for_a_later_start() {
        let starts = Arc::new(ReviewStarts::default());
        keeping(&starts, "rv")
            .send(Err("the candidate is 9 MB; the operator limit is 1".into()))
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            starts.kept("rv", "rev-1").await.as_deref(),
            Some("the candidate is 9 MB; the operator limit is 1")
        );
        // Files that were kept leave nothing to wait on or report.
        keeping(&starts, "kept").send(Ok(())).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(starts.keeping.lock().unwrap().get("kept").is_none());
        assert_eq!(starts.kept("kept", "rev-1").await, None);
    }

    #[tokio::test]
    async fn a_later_revision_never_hears_an_earlier_ones_keep() {
        let starts = Arc::new(ReviewStarts::default());
        keeping(&starts, "rv")
            .send(Err("over the limit".into()))
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Revision 2 kept no files of its own here (auto, or kept already).
        assert_eq!(starts.kept("rv", "rev-2").await, None);
        // And a resubmission or a decision forgets revision 1's.
        starts.unkeep("rv");
        assert_eq!(starts.kept("rv", "rev-1").await, None);
    }
}
