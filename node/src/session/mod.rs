//! Session lifecycle: create a worktree, materialize config, spawn the harness
//! inside the boundary, and hand it to a supervisor.

pub mod attachments;
pub mod chunks;
pub mod exhaustion;
pub mod external;
pub mod materialize;
pub mod placing;
pub mod saved;
pub mod sign_in;
pub mod state;
pub mod supervisor;
pub mod waits;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration, time::Instant};

use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::{
    adapter::{HarnessAdapter, LaunchSpec},
    config::Config,
    runner::Runner,
    session::{
        state::{event_kind as ek, EndReason, SessionState},
        supervisor::{Command, Prompted, Supervisor},
    },
    store::{now_ms, NewEvent, ReviewRow, SessionPatch, SessionRow, Store},
    stream::{Bus, Frame},
};

/// The scheduler's refusal of a session's pod, in its words, when that is
/// why its harness did not launch (`provider::kubernetes::pod::UNSCHEDULABLE`).
fn unschedulable(error: &crate::adapter::AdapterError) -> Option<String> {
    match error {
        crate::adapter::AdapterError::Runner(crate::runner::RunnerError::Refused {
            reason,
            message,
        }) if *reason == crate::provider::kubernetes::pod::UNSCHEDULABLE => Some(message.clone()),
        _ => None,
    }
}

/// Whether its harness did not launch because DigitalOcean no longer has the
/// template its sandbox was to start from
/// (`provider::digitalocean::TEMPLATE_GONE`): the node has forgotten it, so
/// choosing the session's image again starts it elsewhere meanwhile.
fn template_gone(error: &crate::adapter::AdapterError) -> bool {
    matches!(
        error,
        crate::adapter::AdapterError::Runner(crate::runner::RunnerError::Refused { reason, .. })
            if *reason == crate::provider::digitalocean::TEMPLATE_GONE
    )
}

/// Startup is bounded separately from a turn: no model work has happened yet,
/// so a stuck container or ACP handshake is a failed start, not a half-live
/// session.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
const COMMAND_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How many times `[mesh] command_timeout_secs` a create forwarded to a peer
/// is given. The owner answers once the session's row is written, after it
/// has resolved the repository, the project and the harness, which on a
/// managed clone means git, and the answer then comes back through the hub:
/// the generic deadline left too little of that to a reachable owner. The
/// owner holds the create to the same, longer, deadline.
pub const CREATE_TIMEOUT_FACTOR: u32 = 4;

/// How long a create whose owner did not answer in time waits on for the
/// session to arrive anyway, as a mirror or as the owner's late answer,
/// before it says the outcome is unknown.
const LATE_CREATE_GRACE: Duration = Duration::from_secs(3);

/// A request id is the caller's, and it is written on a row and sent to
/// peers: a short token, not text.
fn check_request_id(request_id: &str) -> Result<(), SessionError> {
    let fits = !request_id.is_empty()
        && request_id.len() <= 64
        && request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if fits {
        Ok(())
    } else {
        Err(SessionError::Source(
            "request_id must be 1 to 64 letters, digits, `-` or `_` (a UUID, say)".into(),
        ))
    }
}

/// The creates in flight on this node that carry a request id, one lock per
/// channel and id, so a create sent again while the first is still being
/// made waits for it and then finds its row, rather than racing it to a
/// second session (or to "the task is held by" the first).
#[derive(Default)]
struct RequestLocks(std::sync::Mutex<HashMap<RequestKey, Arc<Mutex<()>>>>);

/// A create's channel and request id.
type RequestKey = (String, String);

impl RequestLocks {
    async fn hold(&self, channel: &str, request_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.0.lock().unwrap();
            // Nobody holds or waits on a lock only the map refers to.
            map.retain(|_, lock| Arc::strong_count(lock) > 1);
            map.entry((channel.to_string(), request_id.to_string()))
                .or_default()
                .clone()
        };
        lock.lock_owned().await
    }
}

/// What a session the node stopped under in the middle of a turn is told
/// when it carries on in the sandbox it ran in.
const CARRY_ON_AFTER_RESTART: &str = "The node restarted during the last turn, which was cut \
     off; the conversation was restored, and the workspace and anything still running in it \
     are as they were. Tool calls that were running when it stopped may not have finished: check \
     what they changed before repeating them, then carry on with the task.";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewSession {
    pub channel: String,
    pub repo_path: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// The forge branch a new branch starts from instead of the default. A
    /// `branch` the forge already has is continued and this is not read.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub work_item_id: Option<String>,
    /// Empty resolves deterministically from the channel phase binding, then
    /// the owner node's offered catalogue. The resolved value is persisted
    /// before the harness starts.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub budget_tokens: Option<i64>,
    /// The first unstructured prompt, queued only after the harness reaches
    /// `running`. Structured work-item and review sessions leave this absent
    /// and are sent the node's kickoff instead.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// The node to run on. Absent or this node: here. Another node: forwarded
    /// to it, which validates and starts the session.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Which phase of the item this session is. Plan and execute need an
    /// item; execute needs the item's plan. Review sessions are spawned by
    /// the node against a review.
    #[serde(default)]
    pub phase: Phase,
    /// Review sessions only: the review to read, and the commit to check out
    /// (the workspace is seeded at it rather than at the source's HEAD, which
    /// for a managed clone is the forge's default branch as just fetched).
    #[serde(default)]
    pub review_id: Option<String>,
    #[serde(default)]
    pub base_sha: Option<String>,
    /// Resume a durable runtime-owned workspace rather than importing a new
    /// selected checkout. The value is a workspace id, never a host path.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Lineage, when this session is made from another one. Written onto the
    /// row at creation and never afterwards. Both are validated against this
    /// node's own sessions: a spec naming a session that is not here is
    /// refused rather than recording a dangling ancestor.
    #[serde(default)]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub continued_from: Option<String>,
    /// The session this one is a fork of (`SessionRow::forked_from`). Not
    /// lineage, and not checked against this node's sessions: a fork started
    /// here from a peer's session names that session, whose row may not have
    /// mirrored here yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// The harness to run this session on. Absent: the node's configured
    /// `[harness] id`. A session runs the harness it names for its whole life;
    /// the node holds an image for every supported one.
    #[serde(default)]
    pub harness: Option<String>,
    /// What this run does if its provider is exhausted, over the channel's
    /// `exhaustion` binding. Absent: the channel's, else pause.
    #[serde(default)]
    pub on_exhaustion: Option<exhaustion::Choice>,
    /// A continuation only: the wait keys of the reviews its first prompt
    /// already told it the operator sent back, as [`told_reviews`] read them
    /// when that prompt was written. Only those are recorded as told when it
    /// takes them over; a review that moved after the prompt was written has
    /// its decision delivered as usual.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub told_reviews: Vec<String>,
    /// The caller's id for this create, so that sending it again after an
    /// answer that never arrived cannot make a second session: the owner
    /// answers a create whose id it already made a session for on the
    /// channel with that session. Absent: every create makes one. A create
    /// forwarded to a peer always carries one, minted here when the caller
    /// gave none. A peer on an older build ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The project the session runs in, when it carries one on from another
    /// session (a moved session keeps its source's). Used only when this node
    /// has that project on the session's channel; otherwise the project is
    /// worked out from the repository as for any session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Plan,
    #[default]
    Execute,
    Review,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Execute => "execute",
            Self::Review => "review",
        }
    }

    /// The phase a session's row records, read back; anything else is
    /// ordinary work.
    pub fn from_row(stored: &str) -> Self {
        match stored {
            "plan" => Self::Plan,
            "review" => Self::Review,
            _ => Self::Execute,
        }
    }
}

/// The phase a reopened session starts in. A plan session reopens as one;
/// anything else reopens as ordinary work on the same workspace. A review
/// session is deliberately not reopened as a review: reviews are spawned by
/// the node against a specific review id, and a second one for a decision
/// that was already made is not what the operator asked for.
fn reopen_phase(stored: &str) -> Phase {
    match stored {
        "plan" => Phase::Plan,
        _ => Phase::Execute,
    }
}

/// What the new session is told, as its first prompt.
///
/// The new harness inherits nothing: not the old one's context window, not
/// its plan, not what it had already tried. Saying so explicitly is the whole
/// point — an agent handed a workspace with no account of how it got that way
/// re-derives it, badly, and calls the result progress.
fn handoff_note(old: &SessionRow, harness: &str, carried: &[ReviewRow]) -> String {
    let why = format!(
        "This session continues {} (id {}, `{}` {}), which ran a harness this node no \
         longer has and which is now archived read-only. You are {harness}, and you \
         inherit none of its context",
        crate::names::session(&old.id),
        old.id,
        old.harness_id,
        old.harness_version,
    );
    handoff(old, &why, carried)
}

/// What the operator asks a session that ended or was suspended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// A message, delivered as its next turn.
    Message(String),
    /// A compaction of its conversation, before anything is asked.
    Compact,
}

/// Whether a session can be asked again in its own conversation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Askable {
    /// Its harness can be started again in the conversation it had.
    pub restorable: bool,
    /// Why it cannot, when it cannot.
    pub reason: Option<String>,
    /// Its harness compacts on request.
    pub compacts: bool,
    /// What refuses it now passes on its own in a moment, so it is worth
    /// asking again without anything else changing.
    pub retry: bool,
}

/// Why a session cannot be asked while its last harness is put away.
const STILL_STOPPING: &str = "it is still stopping; ask it again in a moment";

/// A launch that reopens a session rather than starting one: the
/// conversation it resumes, the row as it was, and whether the operator
/// asked for a compaction.
struct Reopened {
    resume: crate::adapter::Resume,
    from: SessionRow,
    compact: bool,
}

/// The workspace a session works on: one it carried on names it in its path;
/// one seeded for it has its own id.
/// Where a session's harness works in its sandbox: its workspace's mount.
pub const SANDBOX_WORKDIR: &str = "/work";

/// A `workspace_sync` event's payload.
fn workspace_sync_payload(report: &crate::runner::sandbox_files::SyncReport) -> serde_json::Value {
    json!({
        "outcome": report.outcome.as_str(),
        "why": report.why,
        "files": report.files,
        "bytes": report.bytes,
        "ms": report.ms,
        "error": report.error,
        "failures": report.failures,
        "recovered": report.recovered,
        "last_ok_ms": report.last_ok_ms,
        "sandbox": report.sandbox,
    })
}

/// How current the node's copy of a session's sandboxed files is, for the
/// session API: from its running sandbox while one takes trees back, else
/// from its last `workspace_sync` event. `None` for an external session and
/// for one whose files were mounted rather than taken back (nothing syncs
/// them).
pub fn files_sync(store: &Store, node_id: &str, row: &SessionRow) -> Option<serde_json::Value> {
    if row.harness_id == external::HARNESS_ID {
        return None;
    }
    let recorded = store
        .last_event_payload(&row.id, ek::WORKSPACE_SYNC)
        .ok()
        .flatten();
    let live = (row.node_id == node_id)
        .then_some(row.container_name.as_deref())
        .flatten()
        .and_then(crate::runner::sandbox_files::sync_status);
    let last_ok = |p: &serde_json::Value| p["last_ok_ms"].as_i64();
    match (live, recorded) {
        (Some(status), recorded) => {
            let mut view = serde_json::to_value(&status).ok()?;
            // A sandbox taken over from an earlier node process has synced
            // nothing yet under this one: its last record says when it did.
            if status.last_ok_ms.is_none() {
                view["last_ok_ms"] = json!(recorded.as_ref().and_then(last_ok));
            }
            Some(view)
        }
        (None, Some(p)) => Some(json!({
            "last_ok_ms": last_ok(&p),
            "failures": p["failures"].as_u64().unwrap_or(0),
            "last_error": p["error"],
            "in_flight": false,
            "live": false,
            "outcome": p["outcome"],
            "why": p["why"],
        })),
        (None, None) => None,
    }
}

pub(crate) fn workspace_of(row: &SessionRow) -> String {
    row.repo_path
        .strip_prefix("workspace://")
        .unwrap_or(&row.id)
        .to_string()
}

/// What a session carrying on one whose harness died is told: that it died,
/// how, and that whatever it was running in the background went with it.
fn died_note(old: &SessionRow, carried: &[ReviewRow]) -> String {
    let how = old
        .last_error
        .as_deref()
        .filter(|e| !e.is_empty())
        .map(|e| format!(" ({e})"))
        .unwrap_or_default();
    let why = format!(
        "This session continues session {}, whose harness process died in the middle of its \
         work{how} and was not started again. Commands it ran in the background, and their \
         output, are gone. You inherit none of its context",
        old.id,
    );
    handoff(old, &why, carried)
}

/// What a session carrying on a suspended, published one is told: where its
/// work landed, and that what brings it back (a CI failure, a review comment)
/// is on the forge to read.
fn published_note(
    old: &SessionRow,
    published: &[(String, String)],
    carried: &[ReviewRow],
) -> String {
    let landed = published
        .iter()
        .map(|(_, url)| format!("- {url}"))
        .collect::<Vec<_>>()
        .join("\n");
    let why = format!(
        "This session continues session {}, whose work was published and which was then \
         suspended while it sat idle. It published:\n\n{landed}\n\n\
         It is being carried on because something there needs an answer: a CI failure, a \
         review comment, or a change the operator asked for. Read the change on the forge \
         first. You inherit none of the earlier session's context",
        old.id,
    );
    handoff(old, &why, carried)
}

/// What a session carrying on after a node restart is told. Same harness,
/// same workspace, but none of the interrupted conversation: the harness
/// process that held it is gone with the node.
fn restart_note(old: &SessionRow, carried: &[ReviewRow]) -> String {
    let why = format!(
        "This session continues session {}, which was cut off when the node it ran on \
         stopped or restarted in the middle of its work. You inherit none of its context",
        old.id,
    );
    handoff(old, &why, carried)
}

/// What a session carrying on one the node put away while it waited on the
/// operator is told: the handoff, then what the operator decided, as the
/// status tools returned it. A review it takes over is the handoff's to tell,
/// once, and is left out here.
fn held_note(old: &SessionRow, waited: &[waits::Wait], carried: &[ReviewRow]) -> String {
    let why = format!(
        "This session continues session {}, which was waiting on the operator when the node \
         it ran on stopped or restarted, and was put away until they decided. You inherit \
         none of its context",
        old.id,
    );
    let mut note = handoff(old, &why, carried);
    let (open, decided): (Vec<_>, Vec<_>) = waited
        .iter()
        .filter(|w| !carried.iter().any(|r| r.id == w.id))
        .cloned()
        .partition(waits::Wait::is_open);
    if !decided.is_empty() {
        note.push_str(
            "\n\nWhat it was waiting on was decided. Each result below is what its status \
             tool returned; the ids belong to the earlier session.\n",
        );
        note.push_str(&waits::decisions(&decided));
    }
    if !open.is_empty() {
        note.push_str(
            "\n\nStill undecided when this session started, and asked by the earlier session, \
             so not yours to poll — ask again if you still need them:\n\n",
        );
        for wait in &open {
            note.push_str(&format!("- {} `{}` — {}\n", wait.tool, wait.id, wait.about));
        }
    }
    note
}

fn exhausted_note(old: &SessionRow, fallback: Option<&str>, carried: &[ReviewRow]) -> String {
    let why = match fallback {
        Some(model) => format!(
            "This session continues session {}, whose provider was exhausted, on another \
             model ({model}). It stopped at a safe point between turns. You inherit none of \
             its context",
            old.id,
        ),
        None => format!(
            "This session continues session {}, which stopped when its provider was \
             exhausted. It stopped at a safe point between turns. You inherit none of its \
             context",
            old.id,
        ),
    };
    handoff(old, &why, carried)
}

fn handoff(old: &SessionRow, why: &str, carried: &[ReviewRow]) -> String {
    let mut note = format!(
        "{why}: not its conversation, not its plan, not what it had already tried.\n\n\
         What you do have is its workspace, exactly as it left it, on branch `{}`.\n\n\
         Start by reading the workspace — the diff against the branch point, and any \
         notes left in it — and say what you find before changing anything. If the \
         earlier session's transcript matters, it is readable in the interface under \
         that id; ask for what you need from it rather than guessing.",
        old.branch,
    );
    if let Some(item) = old.work_item_id.as_deref() {
        note.push_str(&format!(
            "\n\nIt was working on item {item}, which is still open."
        ));
    }
    note.push_str(&carried_note(carried));
    note
}

/// The wait keys of the `carried` reviews whose notes [`carried_note`] tells:
/// those sent back for changes, as they stood when it read them.
pub fn told_reviews(carried: &[ReviewRow]) -> Vec<String> {
    carried
        .iter()
        .filter(|r| r.state == "revising")
        .map(waits::review_key)
        .collect()
}

/// What a continuation is told about the reviews it takes over from the
/// attempt it continues: each one sent back for changes with the operator's
/// notes, whether an edited diff came with them, and how to resubmit it; each
/// one still waiting, so that it is not opened a second time. Empty when
/// there are none. The notes are told here once: the takeover records this
/// session as told for [`told_reviews`], so they do not come back as a
/// decision too.
pub(crate) fn carried_note(carried: &[ReviewRow]) -> String {
    let mut note = String::new();
    for r in carried {
        let (what, status, submit, key) = if r.kind == crate::store::reports::KIND {
            ("report", "report_status", "submit_report", "report_id")
        } else {
            ("review", "review_status", "submit_review", "review_id")
        };
        if r.state != "revising" {
            note.push_str(&format!(
                "\n\nThe earlier session's {what} `{}` ({}) is still waiting on the \
                 operator. It is yours now, and their decision comes back to you as a \
                 prompt, unless it leaves you nothing to do: {}. If you change the work, \
                 call `{submit}` with that same {key} rather than opening a new {what}.{}",
                r.id,
                r.title,
                if r.kind == crate::store::reports::KIND {
                    "an acknowledgement without a note is only noted at the start of your \
                     next prompt"
                } else {
                    "an approval the node publishes, or an AI reviewer's approval with no \
                     findings, is only noted at the start of your next prompt, so with more to \
                     do after approval keep working or keep calling review_status"
                },
                if r.kind == crate::store::reports::KIND {
                    ""
                } else {
                    " An AI reviewer's advisory verdict on it, once there is one, is in \
                     `reviewer` from review_status."
                },
            ));
            continue;
        }
        let notes = r
            .verdict_reason
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("(no notes given)");
        let (patch, read) = if r.revision_patch.is_some() {
            (
                "An edited diff is attached: apply it to the worktree exactly as given.",
                "the notes and the patch",
            )
        } else {
            ("No edited diff is attached.", "the notes")
        };
        note.push_str(&format!(
            "\n\nThe operator sent the earlier session's {what} `{id}` ({}) back for \
             changes, and it is yours now. Their notes:\n\n{notes}\n\n{patch} Call \
             `{status}` with {key} `{id}` for {read}, make the changes, then call \
             `{submit}` again with that same {key}: the {what} carries on rather than a \
             new one being opened.",
            r.title,
            id = r.id,
        ));
    }
    note
}

/// The first prompt of a session the node starts on structured work: what to
/// do, in a sentence, with the details left to the orientation the harness was
/// already given. `None` for a plain session, which has nothing to start on
/// until the operator says what.
fn kickoff(
    phase: Phase,
    item: Option<&tracon_sync::work::WorkItem>,
    review_id: Option<&str>,
) -> Option<String> {
    let named = |item: &tracon_sync::work::WorkItem| {
        format!("**{}** (`{}`)", item.title, crate::names::task(&item.id))
    };
    match phase {
        Phase::Plan => {
            let item = item?;
            Some(format!(
                "Plan work item {}. It is described in your orientation. Read what you need, \
                 then write the plan as document `{}` with `doc_write`.",
                named(item),
                crate::corpus::work::plan_slug(&item.id),
            ))
        }
        Phase::Execute => {
            let item = item?;
            let plan = match item.phase_plan_slug.as_deref() {
                Some(slug) => format!(
                    "Follow its plan, document `{slug}`: your orientation carries it, and \
                     `doc_read` returns it whole."
                ),
                None => "It is described in your orientation.".to_string(),
            };
            Some(format!(
                "Carry out work item {}. {plan} Commit the work in the worktree and call \
                 `submit_review` when it is ready.",
                named(item),
            ))
        }
        Phase::Review => {
            let review = review_id?;
            Some(format!(
                "Review the change under review `{review}`. The requirements and the diff are \
                 in your orientation. Your workspace holds the candidate's files, without its \
                 Git history, and `{diff}` holds the whole diff. Where the change needs running, \
                 show what you saw with `show_work`. Give your verdict with `review_verdict`.",
                diff = crate::mcp::review::REVIEW_DIFF,
            ))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no usable model: name one, bind the channel phase, or connect a provider")]
    ModelRequired,
    #[error("budget must not be negative")]
    BadBudget,
    /// A budget the session has already spent: it would be held again at
    /// the end of its next turn.
    #[error("the budget must be more than the {0} tokens already spent, or 0 for none")]
    BudgetSpent(i64),
    #[error("node refuses to run harnesses: {0}")]
    NodeRefused(String),
    #[error("harness version mismatch: node expects {pinned}, host has {found}")]
    VersionMismatch { found: String, pinned: String },
    #[error("session not found")]
    NotFound,
    #[error("channel {0} is not one this node holds keys for; create or enroll it first")]
    UnknownChannel(String),
    #[error("channel {0} is archived: bring it back to start work on it again")]
    ChannelArchived(String),
    #[error("a plan session needs a task: pass work_item_id (ready ids: tracon work ready)")]
    WorkItemRequired,
    #[error("task is not ready: {0}")]
    NotReady(String),
    #[error("task is already held by session {0}")]
    InSession(String),
    #[error("execute needs a plan: run a plan session for this task first ({0})")]
    PlanRequired(String),
    #[error("channel is at its daily ceiling: {0}")]
    Ceiling(String),
    /// The session belongs to another node: `(node_id, channel)`.
    #[error("session is owned by node {0}")]
    Remote(String, String),
    /// The owner was known to be unreachable, so nothing was sent to it:
    /// the command did not run there.
    #[error("node {0} is unreachable; nothing was sent to it, so try again when it returns")]
    PeerUnreachable(String),
    /// A command was sent and no answer came in time. It may have run on
    /// the owner, late or with its answer lost on the way back, or not at
    /// all. For a create, `request_id` is the id it was sent with: sending
    /// the create again with it is safe, since the owner answers with the
    /// session the first one made, if it made one.
    #[error("{}", outcome_unknown(.node, .request_id.as_deref()))]
    OutcomeUnknown {
        node: String,
        request_id: Option<String>,
    },
    #[error("this node is not on a mesh; the session's owner cannot be reached")]
    NoMesh,
    #[error("{0}")]
    Rejected(String),
    /// What was sent with a prompt is not something the node stages: too
    /// many files, too large, of a type it does not take, or a session it
    /// has no workspace to stage them into. Nothing was sent.
    #[error("attachment refused: {0}")]
    Attachment(String),
    /// An Allow refused because the plugin's preview, taken again as it was
    /// answered, is not the one the operator read: what the call changes
    /// has moved. Nothing was decided; the card shows the fresh preview.
    #[error("{0}")]
    Stale(String),
    /// What a new session was asked to start from is not something a
    /// session starts from: a host checkout, or a commit.
    #[error("{0}")]
    Source(String),
    /// An operator's answer the node cannot act on as sent, field by field.
    /// Nothing was decided; the operator can fix it and answer again.
    #[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Unfit(Vec<crate::mcp::schema::FieldError>),
    /// Asked to launch, resume, or otherwise run a session whose harness this
    /// build has no adapter for. Carries what to do instead.
    #[error(
        "session {id} ran the retired `{harness}` harness and cannot be launched again. \
             Carry its work forward with `tracon session reopen {id} --harness claude` \
             (or `native`); \
             its workspace and transcript are kept."
    )]
    Legacy { id: String, harness: String },
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// What [`SessionError::OutcomeUnknown`] says: that nothing is known either
/// way, how to check, and how to try again without doing it twice.
fn outcome_unknown(node: &str, request_id: Option<&str>) -> String {
    match request_id {
        Some(request_id) => format!(
            "node {node} did not answer in time, so it is not known whether it created the \
             session: it may still. Check with `tracon work show <task>` (or the sessions \
             list), or send the same create again with request_id {request_id}, which \
             returns the session if one was made rather than making a second \
             (`tracon work start` does this itself)"
        ),
        None => format!(
            "node {node} did not answer in time, so it is not known whether the command ran \
             there: check the session before sending it again"
        ),
    }
}

/// The operator's answer to a card. A harness's permission request reads only
/// the option and arguments; an approval also keeps the reason and notes,
/// which go back to the caller through `approval_status`.
#[derive(Debug, Clone, Default)]
pub struct OperatorAnswer {
    pub option_id: String,
    pub arguments: Option<serde_json::Value>,
    pub reason: Option<String>,
    pub notes: Option<String>,
    /// The hash of the arguments the operator read
    /// ([`crate::store::approvals::arguments_hash`]). When given, an approval
    /// is decided only if it still holds exactly those.
    pub arguments_hash: Option<String>,
    /// The hash of the preview token the operator's card showed
    /// ([`crate::mcp::preview_hash`]). When given, an approval is decided
    /// only if it still keeps that preview's token.
    pub preview_hash: Option<String>,
    /// Who answered, as the audit names them: the operator at the node, in a
    /// signed-in browser, with the token, or through a peer. Not sent on to
    /// the owner of a peer's card, which names the peer itself.
    pub by: Option<String>,
}

impl OperatorAnswer {
    fn into_frame(self, id: &str) -> proto::frame::Command {
        proto::frame::Command::Answer {
            permission_id: id.to_string(),
            option_id: self.option_id,
            arguments: self.arguments,
            reason: self.reason,
            notes: self.notes,
            arguments_hash: self.arguments_hash,
            preview_hash: self.preview_hash,
        }
    }
}

/// A model probe that failed: why, and when.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProbeFailure {
    pub error: String,
    pub at_ms: i64,
}

/// Running sessions, by id. A session that has ended leaves the map; its rows
/// and events stay in the store.
#[derive(Clone)]
pub struct Manager {
    pub(crate) tools: Arc<crate::mcp::Tools>,
    /// Shared with every supervisor and the mesh client, which swaps in a
    /// bundle handed off by a peer.
    policy: Arc<parking_lot::RwLock<crate::policy::Policy>>,
    store: Arc<Store>,
    bus: Bus,
    cfg: Arc<Config>,
    node_id: String,
    live: Arc<Mutex<HashMap<String, mpsc::Sender<Command>>>>,
    /// Session id → (tool token, channel). A token is minted when a session
    /// starts and dropped when it ends, so it authorises exactly one session
    /// for exactly as long as that session runs.
    tokens: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// Which harness processes called under each external lane.
    liveness: Arc<external::Liveness>,
    /// The node's own model probe presents this to the gateway; it may only
    /// read, and it names no channel.
    probe_token: String,
    /// The model probe's last outcome, when it failed: what Settings says
    /// beside its retry. Cleared by a probe that answers.
    probe_failure: Arc<parking_lot::RwLock<Option<ProbeFailure>>>,
    /// Held across one model probe, so two asked for at once (a save and a
    /// boundary check) record their lists in the order they ran.
    probing: Arc<Mutex<()>>,
    /// The hub client, once one exists: commands for sessions other nodes own
    /// are forwarded through it.
    mesh: Arc<std::sync::OnceLock<Arc<crate::mesh::client::MeshClient>>>,
    /// The boundary every harness runs behind.
    backend: Arc<dyn crate::boundary::Backend>,
    /// Provider logins, once the node exists to run them.
    providers: Arc<std::sync::OnceLock<Arc<crate::providers::Providers>>>,
    /// Adapters registered by harness id. Startup registers the ones it
    /// built (and probed); a harness with none registered is built from the
    /// config on demand. A test registers a fake under the id it stands for.
    adapters: Arc<std::sync::Mutex<HashMap<String, Arc<dyn HarnessAdapter>>>>,
    /// The adapter a session that names no harness runs on: the configured
    /// harness's, once startup has probed it.
    default_adapter: Arc<std::sync::OnceLock<Arc<dyn HarnessAdapter>>>,
    previews: Arc<crate::http::preview::PreviewTokens>,
    /// Plugin UIs' capability tokens and listeners (`crate::http::plugin_ui`).
    plugin_uis: Arc<crate::http::plugin_ui::PluginUis>,
    /// The grants whose refusals this node turns into asks, and which an
    /// operator's answer widens. The backend's own, once startup watches it.
    egress: Arc<parking_lot::RwLock<Option<crate::gateway::proxy::Grants>>>,
    /// Required checks `submit_review` started, owned by the node rather
    /// than by the call that asked for them.
    checking: Arc<crate::mcp::checking::Checking>,
    /// The reviews a review session is being started for, so that only one
    /// start at a time reads into a review's workspace.
    review_starts: Arc<crate::mcp::review::ReviewStarts>,
    external_checks: Arc<crate::mcp::review::ExternalChecks>,
    /// What each session has been told of the decisions it waited on.
    told: Arc<waits::Told>,
    /// A restart asked for and waiting: while it waits, new sessions and
    /// new turns are refused.
    restart: Arc<crate::restart::Restart>,
    /// Held while a continuation is checked and started; see [`Continuing`].
    continuing: Arc<Mutex<()>>,
    /// Creates in flight by their request id; see [`RequestLocks`].
    requests: Arc<RequestLocks>,
    /// What counts against `[placement] local_limit`: the live sessions
    /// not placed on DigitalOcean, and the ones placed here still starting.
    slots: Arc<placing::Slots>,
    /// Sessions waiting for a new sign-in to their provider.
    sign_in_waits: Arc<sign_in::SignInWaits>,
    /// The plugin processes running on this node.
    plugins: Arc<crate::plugin::supervisor::Supervisor>,
}

/// The manager's continuation guard, held. Every path that carries a session
/// on looks for an existing continuation and starts its own under it, so the
/// operator's Continue and a decision carrying a held session on (or two
/// clicks) cannot each find none and each start one.
pub(crate) struct Continuing<'a> {
    _held: tokio::sync::MutexGuard<'a, ()>,
}

impl Manager {
    pub fn new(
        store: Arc<Store>,
        bus: Bus,
        cfg: Arc<Config>,
        node_id: String,
        tools: Arc<crate::mcp::Tools>,
        policy: Arc<parking_lot::RwLock<crate::policy::Policy>>,
        backend: Arc<dyn crate::boundary::Backend>,
    ) -> Self {
        let live = cfg.clone();
        let plugins = Arc::new(crate::plugin::supervisor::Supervisor::new(
            store.clone(),
            Config::state_dir(),
            node_id.clone(),
            Arc::new(move |name: &str| {
                let secs = live
                    .live()
                    .plugin
                    .iter()
                    .find(|p| p.name == name)
                    .map(|p| p.idle_timeout_secs())
                    .unwrap_or(crate::config::PluginEntry::DEFAULT_IDLE_TIMEOUT_SECS);
                std::time::Duration::from_secs(secs)
            }),
        ));
        // A plugin's calls back to the node are the tools' to answer.
        let host: std::sync::Weak<dyn crate::plugin::host::Host> = Arc::downgrade(&tools) as _;
        plugins.set_host(host);
        Self {
            plugins,
            tools,
            policy,
            backend,
            store,
            bus,
            cfg,
            node_id,
            live: Arc::new(Mutex::new(HashMap::new())),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            liveness: Arc::default(),
            probe_token: mint_token(),
            probe_failure: Arc::default(),
            probing: Arc::default(),
            mesh: Arc::new(std::sync::OnceLock::new()),
            providers: Arc::new(std::sync::OnceLock::new()),
            adapters: Arc::new(std::sync::Mutex::new(HashMap::new())),
            default_adapter: Arc::new(std::sync::OnceLock::new()),
            previews: Arc::new(crate::http::preview::PreviewTokens::default()),
            plugin_uis: Arc::default(),
            egress: Arc::default(),
            checking: Arc::default(),
            review_starts: Arc::default(),
            external_checks: Arc::default(),
            told: Arc::default(),
            restart: Arc::default(),
            continuing: Arc::default(),
            requests: Arc::default(),
            slots: Arc::default(),
            sign_in_waits: Arc::default(),
        }
    }

    pub(crate) async fn continuing(&self) -> Continuing<'_> {
        Continuing {
            _held: self.continuing.lock().await,
        }
    }

    pub fn restart(&self) -> &Arc<crate::restart::Restart> {
        &self.restart
    }

    /// Refuse new work on this node while a restart waits.
    fn refuse_while_restarting(&self) -> Result<(), SessionError> {
        match self.restart.refusal() {
            Some(why) => Err(SessionError::NodeRefused(why)),
            None => Ok(()),
        }
    }

    /// `refuse_while_restarting`, for a turn of `id`: only one this node
    /// runs. A session on a peer is the peer's to refuse.
    fn refuse_turn_while_restarting(&self, id: &str) -> Result<(), SessionError> {
        if !self.restart.draining() {
            return Ok(());
        }
        match self.store.get_session(id)? {
            Some(row) if row.node_id == self.node_id => self.refuse_while_restarting(),
            _ => Ok(()),
        }
    }

    /// What the sessions here wait on the operator for.
    pub fn waits(&self) -> waits::Waits {
        waits::Waits::new(self.store.clone(), self.checking.clone(), self.told.clone())
    }

    /// The required checks `submit_review` has running, and what the recent
    /// ones came to.
    pub fn checking(&self) -> &Arc<crate::mcp::checking::Checking> {
        &self.checking
    }

    /// The review sessions being started here.
    pub fn review_starts(&self) -> &Arc<crate::mcp::review::ReviewStarts> {
        &self.review_starts
    }

    /// The plugin processes this node runs.
    pub fn plugins(&self) -> &Arc<crate::plugin::supervisor::Supervisor> {
        &self.plugins
    }

    /// The node's checks of external candidates under way on this node.
    pub fn external_checks(&self) -> &Arc<crate::mcp::review::ExternalChecks> {
        &self.external_checks
    }

    pub fn backend(&self) -> &Arc<dyn crate::boundary::Backend> {
        &self.backend
    }

    pub fn cfg(&self) -> &Arc<Config> {
        &self.cfg
    }

    pub fn broker(&self) -> &crate::broker::SharedBroker {
        &self.tools.broker
    }

    /// The identity a channel's commits are authored and committed as: the
    /// account behind its bound forge credential, preferring the forge a
    /// managed clone came from, else the host's own global Git identity.
    /// Bounded in time, so a forge that does not answer delays a launch by
    /// seconds rather than holding it.
    pub async fn commit_identity(
        &self,
        channel: &str,
        repo: &std::path::Path,
    ) -> Option<crate::forge::Identity> {
        let prefer = repo
            .strip_prefix(crate::forge::managed_root(&Config::state_dir()))
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|host| {
                if host.as_os_str().to_string_lossy().contains("gitlab") {
                    crate::forge::Forge::Gitlab
                } else {
                    crate::forge::Forge::Github
                }
            });
        let forge = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::forge::identity_for(&self.tools.broker, channel, &self.node_id, prefer),
        )
        .await
        .ok()
        .flatten();
        match forge {
            Some(identity) => Some(identity),
            None => crate::forge::host_identity(&self.cfg.live().publish.git).await,
        }
    }

    pub fn previews(&self) -> &Arc<crate::http::preview::PreviewTokens> {
        &self.previews
    }

    pub fn plugin_uis(&self) -> &Arc<crate::http::plugin_ui::PluginUis> {
        &self.plugin_uis
    }

    /// Create a session on this node with the node's own adapter, for
    /// sessions the node spawns itself (review sessions).
    /// Carry a session's work forward under a harness this build can run.
    ///
    /// The legacy session is never relaunched and never reused: its id names
    /// a transcript produced by a harness that is gone, and a second session
    /// writing under that id would make the transcript unreadable. This makes
    /// a new session instead — same channel, same workspace, same work item —
    /// records where it came from on both rows' terms (`parent_session`,
    /// `continued_from`), and gives it an explicit handoff note as its first
    /// prompt, because the new harness inherits none of the old one's context
    /// and pretending otherwise is how an agent redoes finished work.
    ///
    /// The new session runs the old one's model unless `model` names another,
    /// and either is checked against `harness` as a session start is.
    pub async fn reopen(
        &self,
        id: &str,
        harness: &str,
        model: Option<&str>,
    ) -> Result<SessionRow, SessionError> {
        let old = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if !crate::adapter::KNOWN.contains(&harness) {
            return Err(SessionError::Rejected(format!(
                "no harness `{harness}`; this node knows {}",
                crate::adapter::KNOWN.join(", ")
            )));
        }
        let model = match model.map(str::trim).filter(|m| !m.is_empty()) {
            Some(named) => named.to_string(),
            None => {
                // As a continuation does: the old model or a refusal, never
                // a quiet swap to the node's default, which can put a local
                // model's work on a credential its harness cannot run. Named
                // here, where `create` could only say no model is usable. A
                // peer's session is checked by the peer.
                if old.node_id == self.node_id
                    && !old.model.trim().is_empty()
                    && !self.model_usable(&old.channel, &old.model, &self.bindings(&old.channel))
                {
                    return Err(SessionError::Rejected(format!(
                        "session {} ran {}, which channel `{}` no longer offers; name another \
                         model to reopen it",
                        old.id, old.model, old.channel
                    )));
                }
                old.model.clone()
            }
        };
        // The old session's workspace, as `continue_work` takes it:
        // seeding afresh would put the old branch name on a new base and
        // leave the work in the old volume behind.
        let workspace_id = old
            .repo_path
            .strip_prefix("workspace://")
            .unwrap_or(&old.id)
            .to_string();
        let carried = self.store.carried_reviews(&old.id)?;
        let note = handoff_note(&old, harness, &carried);
        let spec = NewSession {
            channel: old.channel.clone(),
            repo_path: old.repo_path.clone(),
            branch: Some(old.branch.clone()),
            base: None,
            work_item_id: old.work_item_id.clone(),
            model,
            budget_tokens: Some(old.budget_tokens),
            initial_prompt: Some(note),
            node_id: Some(old.node_id.clone()),
            phase: reopen_phase(&old.phase),
            review_id: None,
            base_sha: None,
            workspace_id: Some(workspace_id),
            parent_session: Some(old.id.clone()),
            continued_from: Some(old.id.clone()),
            forked_from: None,
            harness: Some(harness.to_string()),
            on_exhaustion: None,
            told_reviews: told_reviews(&carried),
            request_id: None,
            project_id: None,
        };
        self.create(spec).await
    }

    /// Carry on the work a node restart interrupted, or that a provider's
    /// exhaustion ended.
    ///
    /// The harness process that held the conversation is gone, so this is a
    /// new session, like `reopen`: same channel, workspace, branch, work item,
    /// model and harness, the lineage on the row, and a handoff note that says
    /// what was lost. Offered once per interrupted session; a second
    /// continuation of the same one is refused and names the first.
    pub async fn continue_interrupted(&self, id: &str) -> Result<SessionRow, SessionError> {
        let held = self.continuing().await;
        self.continue_interrupted_held(&held, id).await
    }

    /// `continue_interrupted`, for a caller already holding the guard.
    pub(crate) async fn continue_interrupted_held(
        &self,
        held: &Continuing<'_>,
        id: &str,
    ) -> Result<SessionRow, SessionError> {
        let old = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        let suspended = old.state == SessionState::Suspended.as_str();
        let carried = self.store.carried_reviews(&old.id)?;
        let note = if let Some(waited) = suspended.then(|| self.held_on(&old.id)).flatten() {
            held_note(&old, &waited, &carried)
        } else if suspended {
            published_note(&old, &self.store.session_publications(&old.id)?, &carried)
        } else {
            match old.end_reason.as_deref() {
                Some(r) if r == state::EndReason::NodeRestart.as_str() => {
                    restart_note(&old, &carried)
                }
                Some(r) if r == state::EndReason::ProviderExhausted.as_str() => {
                    exhausted_note(&old, None, &carried)
                }
                Some(r) if r == state::EndReason::HarnessDied.as_str() => died_note(&old, &carried),
                _ => {
                    return Err(SessionError::Rejected(
                        "only a session a node restart, an exhausted provider or its harness's \
                         death ended, or a suspended one, can be continued this way"
                            .into(),
                    ))
                }
            }
        };
        let choice = self.store.exhaustion(id)?.map(|row| row.choice());
        self.continue_work(held, &old, None, note, told_reviews(&carried), choice)
            .await
    }

    /// The continuation itself: `model` replaces the old session's (a
    /// fallback), and the harness then follows the new model's credential.
    async fn continue_work(
        &self,
        _held: &Continuing<'_>,
        old: &SessionRow,
        model: Option<String>,
        note: String,
        told: Vec<String>,
        choice: Option<exhaustion::Choice>,
    ) -> Result<SessionRow, SessionError> {
        let id = old.id.as_str();
        let suspended = old.state == SessionState::Suspended.as_str();
        if old.harness_id == external::HARNESS_ID {
            return Err(SessionError::Rejected(
                "an external agent's work runs outside this node; start it again where it runs"
                    .into(),
            ));
        }
        if old.phase == Phase::Review.as_str() {
            return Err(SessionError::Rejected(
                "a review session is started by its review, not continued; resubmit the review"
                    .into(),
            ));
        }
        if let Some(next) = self
            .store
            .list_sessions(None)?
            .into_iter()
            .find(|s| s.continued_from.as_deref() == Some(id))
        {
            return Err(SessionError::Rejected(format!(
                "{} already continues this one",
                crate::names::session(&next.id)
            )));
        }
        // A session on a checkout keeps its workspace under its own id; one
        // already on a retained workspace names it in its path.
        let workspace_id = old
            .repo_path
            .strip_prefix("workspace://")
            .unwrap_or(&old.id)
            .to_string();
        let harness = match &model {
            Some(_) => None,
            None => crate::adapter::KNOWN
                .contains(&old.harness_id.as_str())
                .then(|| old.harness_id.clone()),
        };
        let spec = NewSession {
            channel: old.channel.clone(),
            repo_path: old.repo_path.clone(),
            branch: Some(old.branch.clone()),
            base: None,
            work_item_id: old.work_item_id.clone(),
            model: model.unwrap_or_else(|| old.model.clone()),
            budget_tokens: Some(old.budget_tokens),
            initial_prompt: Some(note),
            node_id: Some(old.node_id.clone()),
            phase: reopen_phase(&old.phase),
            review_id: None,
            base_sha: None,
            workspace_id: Some(workspace_id),
            parent_session: Some(old.id.clone()),
            continued_from: Some(old.id.clone()),
            forked_from: None,
            harness,
            on_exhaustion: choice,
            told_reviews: told,
            request_id: None,
            project_id: None,
        };
        let row = self.create(spec).await?;
        // A suspended session's work now lives in the new one; the operator
        // asked for that, so the old one ends, saying where it went.
        if suspended {
            self.hand_over_suspended(&old.id, &row.id).await?;
        }
        Ok(row)
    }

    /// Why `row`'s own conversation cannot be picked up again, for the
    /// operator to read, or the adapter that would pick it up. Asked of a
    /// session that ended or was suspended; anything here refuses before the
    /// row is touched, and the operator is offered a continuation instead.
    async fn reopenable(&self, row: &SessionRow) -> Result<Arc<dyn HarnessAdapter>, String> {
        if row.node_id != self.node_id {
            return Err(format!(
                "it ran on {}; ask it there",
                self.store.node_display_name(&row.node_id)
            ));
        }
        if row.legacy_ms.is_some() {
            return Err(format!(
                "it ran the `{}` harness, which this node no longer has",
                row.harness_id
            ));
        }
        if row.archived_ms.is_some() {
            return Err("it is archived; bring it back to ask it".into());
        }
        let state = SessionState::from_stored(&row.state);
        if state != SessionState::Suspended && !state.is_terminal() {
            return Err(format!("it is {}; send it a prompt", row.state));
        }
        // Its last harness is still being put away, and holds the state a
        // relaunch would stage.
        if self.live.lock().await.contains_key(&row.id) || materialize::holds_state(&row.id) {
            return Err(STILL_STOPPING.into());
        }
        if row.harness_id == external::HARNESS_ID {
            return Err("an external agent's conversation is held where it runs".into());
        }
        if row.phase == Phase::Review.as_str() {
            return Err("a review session is started by its review, not asked again".into());
        }
        let adapter = self
            .adapter_by_id(&row.harness_id)
            .map_err(|e| e.to_string())?;
        if !adapter.reopens() {
            return Err(format!(
                "the `{}` harness cannot pick a conversation up again once its process is gone",
                row.harness_id
            ));
        }
        if row.harness_session_id.is_none() {
            return Err("its harness never started a conversation".into());
        }
        if row.budget_tokens > 0 && row.tokens_used >= row.budget_tokens {
            return Err(format!(
                "it spent its token budget ({} of {} tokens)",
                row.tokens_used, row.budget_tokens
            ));
        }
        if !self.bindings(&row.channel)["archived"].is_null() {
            return Err(format!("its channel {} is archived", row.channel));
        }
        // Closing the task ends its sessions; asking one again would run it
        // on a task that is done.
        if let Some(item_id) = row.work_item_id.as_deref().filter(|i| !i.trim().is_empty()) {
            let closed = self
                .store
                .work_status(&row.channel, None)
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|v| v.item.id == item_id)
                .is_some_and(|v| matches!(v.readiness, tracon_sync::work::Readiness::Closed));
            if closed {
                return Err("its task is closed".into());
            }
        }
        // A conversation the harness was asked for and had no record of
        // stays gone; anything else that failed a reopening may not.
        if let Ok(Some(last)) = self
            .store
            .last_event_payload(&row.id, ek::CONVERSATION_RESUMED)
        {
            if last["gone"] == true {
                return Err(last["reason"]
                    .as_str()
                    .unwrap_or("the harness has no record of its conversation")
                    .to_string());
            }
        }
        let workspace = workspace_of(row);
        let others = self.store.list_sessions(None).map_err(|e| e.to_string())?;
        if let Some(other) = others.iter().find(|other| {
            other.id != row.id
                && other.node_id == self.node_id
                && !SessionState::from_stored(&other.state).is_terminal()
                && (workspace_of(other) == workspace
                    || (row.work_item_id.is_some()
                        && other.phase != Phase::Review.as_str()
                        && other.work_item_id == row.work_item_id))
        }) {
            return Err(format!(
                "{} is {} on its workspace or its task now",
                crate::names::session(&other.id),
                other.state.replace('_', " ")
            ));
        }
        if !self
            .backend
            .volume_exists(&crate::workspace::volume_name(&workspace))
            .await
        {
            return Err("its workspace was removed".into());
        }
        Ok(adapter)
    }

    /// Whether the operator can ask `id` again in its own conversation, and
    /// compact it; with the reason when it cannot be asked.
    pub async fn askable(&self, id: &str) -> Result<Askable, SessionError> {
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        // A peer's live session is compacted where it runs; the mesh carries
        // no compaction to forward.
        let compacts = row.harness_id != external::HARNESS_ID
            && row.node_id == self.node_id
            && self
                .adapter_by_id(&row.harness_id)
                .is_ok_and(|adapter| adapter.compacts());
        let reason = self.reopenable(&row).await.err();
        Ok(Askable {
            restorable: reason.is_none(),
            retry: reason.as_deref() == Some(STILL_STOPPING),
            reason,
            compacts,
        })
    }

    /// Ask a session that ended or was suspended: the same row is reopened,
    /// its harness started again in its own conversation on its own
    /// workspace, and `ask` delivered as its first turn through the
    /// supervisor's usual path, so the budget and policy apply as to any
    /// other. Not a continuation: `continued_from` stays empty and nothing is
    /// added to the lineage.
    pub async fn ask(&self, id: &str, ask: Ask) -> Result<SessionRow, SessionError> {
        self.reopen_as(id, Some(ask)).await
    }

    /// [`Self::ask`], or with `None` reopened with nothing asked: its
    /// harness started again on its conversation, waiting for a turn.
    async fn reopen_as(&self, id: &str, ask: Option<Ask>) -> Result<SessionRow, SessionError> {
        self.refuse_while_restarting()?;
        // Held from the checks to the reopening, so two asks of sessions on
        // one workspace, or an ask and a Continue, cannot each find it free.
        let continuing = self.continuing().await;
        let was = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        Self::refuse_legacy(&was)?;
        let adapter = self.reopenable(&was).await.map_err(|why| {
            SessionError::Rejected(format!("its conversation cannot be restored: {why}"))
        })?;
        if matches!(ask, Some(Ask::Compact)) && !adapter.compacts() {
            return Err(SessionError::Rejected(format!(
                "the `{}` harness does not compact on request",
                adapter.id()
            )));
        }
        let harness_session_id = was
            .harness_session_id
            .clone()
            .ok_or_else(|| SessionError::Rejected("its harness never started".into()))?;
        let message = match &ask {
            Some(Ask::Message(text)) if text.trim().is_empty() => {
                return Err(SessionError::Rejected("say what to ask it".into()))
            }
            Some(Ask::Message(text)) => Some(text.clone()),
            Some(Ask::Compact) | None => None,
        };
        if !self.store.reopen_session(id, &was.state)? {
            return Err(SessionError::Rejected(
                "it changed while it was being reopened; look again".into(),
            ));
        }
        drop(continuing);
        // Kept as the box's text until the supervisor takes it, as a new
        // session's first prompt is.
        if let Some(text) = &message {
            self.store.set_draft(id, Some(text))?;
        }
        let spec = NewSession {
            channel: was.channel.clone(),
            repo_path: was.repo_path.clone(),
            branch: Some(was.branch.clone()),
            base: None,
            work_item_id: was.work_item_id.clone(),
            model: was.model.clone(),
            budget_tokens: Some(was.budget_tokens),
            initial_prompt: message,
            node_id: None,
            phase: Phase::from_row(&was.phase),
            review_id: was.review_id.clone(),
            base_sha: None,
            workspace_id: Some(workspace_of(&was)),
            parent_session: None,
            continued_from: None,
            forked_from: None,
            harness: Some(was.harness_id.clone()),
            on_exhaustion: None,
            told_reviews: Vec::new(),
            request_id: None,
            project_id: None,
        };
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        self.bus.publish(Frame::Session(Box::new(row.clone())));
        let reopened = Reopened {
            resume: crate::adapter::Resume {
                harness_session_id,
                last_seq: 0,
            },
            from: was.clone(),
            compact: matches!(ask, Some(Ask::Compact)),
        };
        let this = self.clone();
        let id = id.to_string();
        let started = Instant::now();
        let slug = id.split('-').next_back().unwrap_or("session").to_string();
        tokio::spawn(async move {
            let branch = spec.branch.clone().unwrap_or_default();
            let Err(e) = this
                .start(
                    &id,
                    spec,
                    branch,
                    slug,
                    adapter,
                    started,
                    ("session", "session"),
                    false,
                    Some(reopened),
                )
                .await
            else {
                return;
            };
            tracing::warn!(session = %id, error = %e, "the session could not be asked again");
            materialize::release_state(&id);
            this.tokens.lock().await.remove(&id);
            // Back to how it ended: a reopening that never started changed
            // nothing about the session, and its ending is still the one
            // that stands. A Stop that landed meanwhile stands instead.
            let _ = this.store.restore_session(&id, &was);
            let gone = matches!(
                e.downcast_ref::<crate::adapter::AdapterError>(),
                Some(crate::adapter::AdapterError::ConversationGone(_))
            );
            this.record(NewEvent {
                session_id: id.clone(),
                work_item_id: None,
                kind: ek::CONVERSATION_RESUMED.into(),
                ref_id: None,
                payload: json!({
                    "outcome": "failed",
                    "reason": e.to_string(),
                    "gone": gone,
                    "from_state": was.state,
                    "end_reason": was.end_reason,
                }),
                at_ms: now_ms(),
                mono_ms: started.elapsed().as_millis() as i64,
            });
            if let Ok(Some(row)) = this.store.get_session(&id) {
                this.bus.publish(Frame::Session(Box::new(row)));
            }
        });
        Ok(row)
    }

    /// Compact a session's conversation: a live one's at once, between
    /// turns; one that ended or was suspended by reopening it to compact
    /// (`ask`), after which it is live and takes the operator's question.
    pub async fn compact(&self, id: &str) -> Result<(), SessionError> {
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        let state = SessionState::from_stored(&row.state);
        if state == SessionState::Suspended || state.is_terminal() {
            return self.ask(id, Ask::Compact).await.map(|_| ());
        }
        self.ensure_active(id)?;
        self.refuse_turn_while_restarting(id)?;
        if row.node_id != self.node_id {
            return Err(SessionError::Rejected(format!(
                "it runs on {}; compact it there",
                self.store.node_display_name(&row.node_id)
            )));
        }
        if !self
            .adapter_by_id(&row.harness_id)
            .is_ok_and(|adapter| adapter.compacts())
        {
            return Err(SessionError::Rejected(format!(
                "the `{}` harness does not compact on request",
                row.harness_id
            )));
        }
        let (ack, wait) = oneshot::channel();
        self.send(id, Command::Compact { ack }).await?;
        wait.await
            .map_err(|_| SessionError::Rejected("session stopped".into()))?
            .map_err(SessionError::Rejected)
    }

    /// The contexts this session saved before its compactions and restarts
    /// (`saved`), oldest first. Read from this node's copy of its harness
    /// state; a session that runs on another node keeps its saves there.
    pub async fn saved_contexts(&self, id: &str) -> Result<Vec<saved::Manifest>, SessionError> {
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if row.node_id != self.node_id {
            return Err(SessionError::Remote(row.node_id, row.channel));
        }
        saved::list(self.backend.as_ref(), id)
            .await
            .map_err(SessionError::Rejected)
    }

    /// One saved context, with its files' contents when `with_contents`
    /// (an export). `NotFound` when the session has no save of that index.
    pub async fn saved_context(
        &self,
        id: &str,
        index: u32,
        with_contents: bool,
    ) -> Result<(saved::Saved, Vec<serde_json::Value>), SessionError> {
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if row.node_id != self.node_id {
            return Err(SessionError::Remote(row.node_id, row.channel));
        }
        saved::read(self.backend.as_ref(), id, index, with_contents)
            .await
            .map_err(SessionError::Rejected)?
            .ok_or(SessionError::NotFound)
    }

    /// End the suspended session `old` as continued by `new`. One that ended
    /// some other way while `new` started (its item closed, or it was
    /// stopped) is not carried on after all: `new` is stopped again and
    /// given back what it took over.
    pub(crate) async fn hand_over_suspended(
        &self,
        old: &str,
        new: &str,
    ) -> Result<(), SessionError> {
        if self.end_suspended(
            old,
            state::EndReason::Continued,
            json!({ "continued_as": new }),
        )? {
            return Ok(());
        }
        let _ = self.stop(new).await;
        let _ = self.store.hand_over_reviews(new, old);
        Err(SessionError::Rejected(format!(
            "session {old} ended while it was being carried on"
        )))
    }

    /// A suspended session here whose item has closed ends with it. Closing
    /// the item ends it at once (`item_closed`), but a close can miss it: one
    /// sent while it was being suspended reached a supervisor that had
    /// stopped reading, and one made on another node arrives as a replicated
    /// change with nothing to end the session that holds it.
    pub(crate) fn end_if_item_closed(&self, id: &str) -> bool {
        let Ok(Some(row)) = self.store.get_session(id) else {
            return false;
        };
        if row.node_id != self.node_id || row.state != SessionState::Suspended.as_str() {
            return false;
        }
        let closed = row
            .work_item_id
            .as_deref()
            .and_then(|item| self.store.work_get(item).ok().flatten())
            .is_some_and(|item| item.state == tracon_sync::work::CLOSED);
        closed
            && self
                .end_suspended(id, EndReason::ItemClose, json!({}))
                .unwrap_or(false)
    }

    /// `end_if_item_closed` for every suspended session here.
    pub async fn end_closed_suspended(&self) {
        let Ok(rows) = self
            .store
            .list_sessions(Some(SessionState::Suspended.as_str()))
        else {
            return;
        };
        for row in rows.iter().filter(|row| row.node_id == self.node_id) {
            self.end_if_item_closed(&row.id);
        }
    }

    /// Close a session whose work moved to `to_session` on `to_node`, which
    /// confirmed it had imported transfer `transfer_id`. A live session ends
    /// now; a suspended, paused or starting one is closed where it is. Its
    /// reviews are left as they are, and its workspace is kept until gc.
    /// `false` when it had already ended (a repeated confirmation).
    pub async fn end_moved(
        &self,
        id: &str,
        to_session: &str,
        to_node: &str,
        transfer_id: &str,
    ) -> Result<bool, SessionError> {
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if row.node_id != self.node_id {
            return Err(SessionError::Remote(row.node_id, row.channel));
        }
        if SessionState::from_stored(&row.state).is_terminal() {
            return Ok(false);
        }
        let detail = json!({
            "to_session": to_session,
            "to_node": to_node,
            "transfer_id": transfer_id,
        });
        self.record_event(id, ek::MOVED, detail.clone());
        if self.send(id, Command::End(EndReason::Moved)).await.is_ok() {
            return Ok(true);
        }
        if self.end_suspended(id, EndReason::Moved, detail)? {
            return Ok(true);
        }
        // No supervisor: a paused session, or one still starting, which
        // observes the terminal row as a stop.
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if SessionState::from_stored(&row.state).is_terminal() {
            return Ok(false);
        }
        self.store.update_session(
            id,
            SessionPatch {
                state: Some(SessionState::Closed.as_str().into()),
                end_reason: Some(EndReason::Moved.as_str().into()),
                ended_mono_ms: Some(0),
                turn_active: Some(false),
                ..Default::default()
            },
        )?;
        if let Some(container) = row.container_name {
            let runner = self.backend.runner(Vec::new());
            let _ = tokio::time::timeout(
                CLEANUP_TIMEOUT,
                crate::sidecars::stop_all(runner.as_ref(), &self.store, id),
            )
            .await;
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
        }
        self.record_event(
            id,
            ek::STATE,
            json!({ "state": "closed", "end_reason": EndReason::Moved.as_str() }),
        );
        if let Ok(Some(row)) = self.store.get_session(id) {
            self.bus.publish(Frame::Session(Box::new(row)));
        }
        Ok(true)
    }

    /// End a suspended session, which has no supervisor to end it: the row
    /// closes with `reason`, and its `state` event carries `detail` besides.
    /// `false` when it was no longer suspended.
    pub(crate) fn end_suspended(
        &self,
        id: &str,
        reason: state::EndReason,
        detail: serde_json::Value,
    ) -> Result<bool, SessionError> {
        let closed = self.store.update_session_if(
            id,
            SessionState::Suspended.as_str(),
            SessionPatch {
                state: Some(SessionState::Closed.as_str().into()),
                end_reason: Some(reason.as_str().into()),
                ..Default::default()
            },
        )?;
        if closed {
            if let Ok(Some(row)) = self.store.get_session(id) {
                crate::telemetry::telemetry().session_ended(
                    &crate::telemetry::SessionSeries::of_row(&self.store, &row),
                    reason.as_str(),
                    None,
                );
            }
            let mut payload = json!({ "state": "closed", "end_reason": reason.as_str() });
            if let (Some(payload), Some(detail)) = (payload.as_object_mut(), detail.as_object()) {
                payload.extend(detail.clone());
            }
            self.record_event(id, ek::STATE, payload);
            if let Ok(Some(row)) = self.store.get_session(id) {
                self.bus.publish(Frame::Session(Box::new(row)));
            }
        }
        Ok(closed)
    }

    /// A continuation takes over the reviews the attempt it continues left
    /// open or sent back for changes, whichever path started it, before its
    /// harness runs: from here its waits, the operator's decision and the
    /// right to resubmit each one are the new session's. A session continuing
    /// none of them gets none, so ownership still refuses everyone else.
    ///
    /// A review sent back for changes is already told when the handoff carried
    /// its notes, and bringing them back as a decision would say them twice.
    /// Only the keys in `told` are recorded, the reviews as the handoff read
    /// them: one that moved between the handoff being written and this
    /// takeover (still waiting then, sent back now) has a key the handoff
    /// never named, so its decision is delivered by `settle_waits` instead of
    /// being lost. Candidates keep the session that captured them, and checks
    /// the earlier attempt left running stay under its id, cancelled as it
    /// ends.
    async fn take_over_reviews(
        &self,
        from: &str,
        to: &str,
        told: &[String],
    ) -> Result<(), SessionError> {
        let moved = self.store.hand_over_reviews(from, to)?;
        if moved.is_empty() {
            return Ok(());
        }
        for review in &moved {
            let key = waits::review_key(review);
            if told.contains(&key) {
                self.waits().tell(to, &key);
            }
            // The handoff points it at the AI reviewer's verdict on a review
            // still waiting (`carried_note`), so that verdict is not brought
            // back as a prompt as well; one given after this still is. Its
            // findings are left unsent, so that review_status carries them.
            if let Some(verdict) = crate::mcp::review::current_verdict(&self.store, review) {
                self.waits()
                    .tell(to, &waits::reviewer_key(&review.id, &verdict));
            }
            self.record(NewEvent {
                session_id: to.to_string(),
                work_item_id: None,
                kind: ek::REVIEW_TAKEN_OVER.to_string(),
                ref_id: Some(review.id.clone()),
                payload: json!({
                    "review_id": review.id,
                    "state": review.state,
                    "from_session": from,
                    "to_session": to,
                }),
                at_ms: now_ms(),
                mono_ms: 0,
            });
        }
        self.publish_queue().await;
        Ok(())
    }

    /// What a suspended session was put away waiting on, as those waits stand
    /// now; `None` for one put away for sitting idle.
    fn held_on(&self, id: &str) -> Option<Vec<waits::Wait>> {
        let payload = self
            .store
            .last_event_payload(id, ek::STATE)
            .ok()
            .flatten()?;
        if payload["state"] != SessionState::Suspended.as_str() {
            return None;
        }
        let ids: Vec<&str> = payload["waiting_on"]
            .as_array()?
            .iter()
            .filter_map(|w| w["id"].as_str())
            .collect();
        Some(
            self.waits()
                .of(id)
                .into_iter()
                .filter(|w| ids.contains(&w.id.as_str()))
                .collect(),
        )
    }

    /// Something `session_id` waits on may have been decided. A live session
    /// looks now; one put away waiting on the operator is carried on once
    /// nothing it waits on is open.
    pub async fn wait_moved(&self, session_id: &str) {
        if self.send(session_id, Command::WaitMoved).await.is_ok() {
            return;
        }
        if let Ok(Some(row)) = self.store.get_session(session_id) {
            if row.node_id == self.node_id && row.state == SessionState::Suspended.as_str() {
                // Starting a session is not the deciding caller's to wait on.
                let manager = self.clone();
                tokio::spawn(async move {
                    manager.continue_held_one(&row).await;
                });
            }
        }
    }

    /// Carry on each session here that was put away waiting on the operator
    /// — the node stopped under it — once everything it waited on has been
    /// decided: a new session on its workspace, told each decision.
    pub async fn continue_held(&self) {
        // Carried on by the node that comes back, not into a restart.
        if self.restart.draining() {
            return;
        }
        let Ok(rows) = self
            .store
            .list_sessions(Some(SessionState::Suspended.as_str()))
        else {
            return;
        };
        for row in rows.iter().filter(|row| row.node_id == self.node_id) {
            self.continue_held_one(row).await;
        }
    }

    async fn continue_held_one(&self, old: &SessionRow) -> Option<SessionRow> {
        // The sweep and a decision can both get here for one session; it is
        // carried on once.
        let held = self.continuing().await;
        let old = self.store.get_session(&old.id).ok().flatten()?;
        if old.state != SessionState::Suspended.as_str() {
            return None;
        }
        let old = &old;
        let waited = self.held_on(&old.id)?;
        if !self.waits().open(&old.id).is_empty() {
            return None;
        }
        // Decisions that leave nothing to act on (`waits::needs_turn`) carry
        // nothing on: an approval the node published leaves it an idle
        // published session, which the operator continues from the
        // continuation view like any other. Its state is recorded again
        // without what it waited on, so it is not held any more.
        let decided: Vec<_> = waited.iter().filter(|w| !w.is_open()).collect();
        if !decided.is_empty() && !decided.iter().any(|w| waits::needs_turn(w)) {
            self.record(NewEvent {
                session_id: old.id.clone(),
                work_item_id: None,
                kind: ek::STATE.to_string(),
                ref_id: None,
                payload: json!({
                    "state": SessionState::Suspended.as_str(),
                    "end_reason": null,
                }),
                at_ms: now_ms(),
                mono_ms: 0,
            });
            return None;
        }
        let carried = self.store.carried_reviews(&old.id).ok()?;
        let note = held_note(old, &waited, &carried);
        match self
            .continue_work(&held, old, None, note, told_reviews(&carried), None)
            .await
        {
            Ok(row) => Some(row),
            Err(error) => {
                tracing::warn!(session = %old.id, %error, "could not carry on a session whose waits were decided");
                None
            }
        }
    }

    /// Put away every live session here whose work is published and which has
    /// sat idle for `idle`: no turn, nothing waiting, nothing recorded. Its
    /// supervisor stops the harness and container and its egress grant goes
    /// with it; the workspace stays, and so does the row, as `suspended`.
    pub async fn suspend_idle_published(&self, idle: Duration) {
        let ids: Vec<String> = self.live.lock().await.keys().cloned().collect();
        let now = now_ms();
        for id in ids {
            let Ok(Some(row)) = self.store.get_session(&id) else {
                continue;
            };
            if row.state != SessionState::Running.as_str() || row.turn_active != 0 {
                continue;
            }
            if self
                .store
                .session_publications(&id)
                .map(|p| p.is_empty())
                .unwrap_or(true)
            {
                continue;
            }
            let waiting = self
                .store
                .open_permissions()
                .unwrap_or_default()
                .iter()
                .any(|p| p.session_id.as_deref() == Some(id.as_str()));
            // Nor is one waiting on the operator's decision, which comes
            // back to it as a prompt.
            if waiting || !self.waits().open(&id).is_empty() {
                continue;
            }
            let last = self
                .store
                .last_event_ms(&id)
                .ok()
                .flatten()
                .unwrap_or(row.updated_ms)
                .max(row.updated_ms);
            let idle_ms = now - last;
            if idle_ms < idle.as_millis() as i64 {
                continue;
            }
            let _ = self.send(&id, Command::Suspend { idle_ms }).await;
        }
    }

    /// Register an adapter under its own id. Node-spawned sessions on that
    /// harness use it; the node's default is registered at startup, once it
    /// has been probed.
    pub fn set_adapter(&self, adapter: Arc<dyn HarnessAdapter>) {
        self.adapters
            .lock()
            .unwrap()
            .insert(adapter.id().to_string(), adapter);
    }

    /// The adapter for sessions that name no harness. Set once; the first
    /// caller wins, which is startup in the node and the harness in a test.
    pub fn set_default_adapter(&self, adapter: Arc<dyn HarnessAdapter>) {
        let _ = self.default_adapter.set(adapter);
    }

    /// What a model's credential is, as far as choosing a harness goes: the
    /// sign-in that serves `channel`, or without one the provider's primary
    /// sign-in. `None` when the node cannot say: a provider it has no
    /// configuration for, or a credential it does not hold.
    fn credential_class(
        &self,
        model: &str,
        channel: Option<&str>,
    ) -> Option<crate::adapter::compat::CredentialClass> {
        use crate::adapter::compat::CredentialClass;
        let Some((provider_name, _)) = model.split_once('/') else {
            return (!model.trim().is_empty()).then_some(CredentialClass::BareAlias);
        };
        let provider = self.cfg.providers.get(provider_name)?;
        let broker = self.tools.broker.read().ok()?;
        let base = provider.credential.as_str();
        let credential = channel
            .and_then(|channel| {
                broker.model_credential(provider_name, base, channel, &self.node_id)
            })
            .or_else(|| broker.primary_sign_in(provider_name, base, &self.node_id))
            .map(|(_, credential)| credential)
            .or_else(|| broker.get(base))?;
        Some(CredentialClass::classify(&provider.shape, &credential.kind))
    }

    /// The supported harnesses `model` runs on, for the picker; empty where
    /// the node cannot classify it.
    pub fn model_harnesses(&self, model: &str) -> Vec<&'static str> {
        self.credential_class(model, None)
            .map(|class| class.harnesses_among(&crate::adapter::enabled(&self.cfg)))
            .unwrap_or_default()
    }

    /// The harness a session runs on and why: the one the spec names, if the
    /// model's credential runs there; otherwise the only one it runs on; and
    /// where it runs on either (an Anthropic API key) or the node cannot say,
    /// the configured `[harness] id`.
    fn resolve_harness(
        &self,
        spec: &NewSession,
    ) -> Result<(Arc<dyn HarnessAdapter>, &'static str), SessionError> {
        let class = self.credential_class(&spec.model, Some(&spec.channel));
        if let Some(named) = spec.harness.as_deref().filter(|h| !h.trim().is_empty()) {
            if !crate::adapter::is_enabled(&self.cfg, named) {
                return Err(SessionError::Rejected(format!(
                    "unknown harness `{named}`; this node runs {}",
                    crate::adapter::enabled(&self.cfg).join(", ")
                )));
            }
            self.check_fit(&spec.model, class, named)?;
            return Ok((self.adapter_by_id(named)?, "explicit"));
        }
        let Some(class) = class else {
            return Ok((self.default_adapter()?, "node_default"));
        };
        match class
            .harnesses_among(&crate::adapter::enabled(&self.cfg))
            .as_slice()
        {
            [only] => Ok((self.adapter_by_id(only)?, "credential")),
            [] => Err(SessionError::Rejected(format!(
                "no supported harness runs {}",
                spec.model
            ))),
            both if both.contains(&self.cfg.harness.id.as_str()) => {
                Ok((self.default_adapter()?, "node_default"))
            }
            [first, ..] => Ok((self.adapter_by_id(first)?, "credential")),
        }
    }

    fn check_fit(
        &self,
        model: &str,
        class: Option<crate::adapter::compat::CredentialClass>,
        harness: &str,
    ) -> Result<(), SessionError> {
        match class.map(|class| class.fits(harness)) {
            Some(Err(reason)) => Err(SessionError::Rejected(format!(
                "{model} cannot run on the `{harness}` harness: {reason}"
            ))),
            _ => Ok(()),
        }
    }

    /// The configured harness's adapter: the one startup probed, once it has.
    fn default_adapter(&self) -> Result<Arc<dyn HarnessAdapter>, SessionError> {
        if let Some(default) = self.default_adapter.get() {
            return Ok(default.clone());
        }
        self.adapter_by_id(&self.cfg.harness.id)
    }

    /// Every supported harness has an image on this node, so the answer
    /// never depends on which one is the default.
    fn adapter_by_id(&self, harness: &str) -> Result<Arc<dyn HarnessAdapter>, SessionError> {
        if let Some(default) = self.default_adapter.get().filter(|a| a.id() == harness) {
            return Ok(default.clone());
        }
        if let Some(registered) = self.adapters.lock().unwrap().get(harness) {
            return Ok(registered.clone());
        }
        crate::adapter::adapter_for_id(&self.cfg, harness)
            .map_err(|e| SessionError::Rejected(e.to_string()))
    }

    pub fn set_mesh(&self, mesh: Arc<crate::mesh::client::MeshClient>) {
        let _ = self.mesh.set(mesh);
    }

    pub fn mesh(&self) -> Option<&Arc<crate::mesh::client::MeshClient>> {
        self.mesh.get()
    }

    pub fn set_providers(&self, p: Arc<crate::providers::Providers>) {
        let _ = self.providers.set(p);
    }

    pub fn providers(&self) -> Option<&Arc<crate::providers::Providers>> {
        self.providers.get()
    }

    /// Run a command on the node that owns a session. A prompt to an owner
    /// that is unreachable is queued and delivered when it returns (the
    /// operator asked for it; the outbox keeps it); anything else must be
    /// answered now or fail, because the operator is waiting on the result.
    ///
    /// Every command sent here changes something on the owner, so silence
    /// is not a failure: an owner known to be away is not sent the command
    /// at all ([`SessionError::PeerUnreachable`]), and one that was sent it
    /// and did not answer in time may have run it
    /// ([`SessionError::OutcomeUnknown`]).
    async fn forward(
        &self,
        node_id: &str,
        command: proto::frame::Command,
        queue_if_unreachable: bool,
    ) -> Result<serde_json::Value, SessionError> {
        let mesh = self.mesh.get().ok_or(SessionError::NoMesh)?;
        let mut timeout = Duration::from_secs(self.cfg.mesh.command_timeout_secs.max(1));
        if matches!(command, proto::frame::Command::Create { .. }) {
            timeout *= CREATE_TIMEOUT_FACTOR;
        }
        if !mesh.peer_reachable(node_id) {
            if !queue_if_unreachable {
                return Err(SessionError::PeerUnreachable(node_id.to_string()));
            }
            mesh.send_command(node_id, command)
                .map_err(|e| SessionError::Rejected(e.to_string()))?;
            return Ok(json!({ "queued_for_owner": true }));
        }
        match mesh.command(node_id, command, timeout).await {
            Ok(v) => Ok(v),
            Err(crate::mesh::forward::CommandError::Timeout) => Err(SessionError::OutcomeUnknown {
                node: node_id.to_string(),
                request_id: None,
            }),
            Err(
                e @ crate::mesh::forward::CommandError::OlderPeer { .. }
                | e @ crate::mesh::forward::CommandError::Refused(_)
                | e @ crate::mesh::forward::CommandError::Local(_),
            ) => Err(SessionError::Rejected(e.to_string())),
        }
    }

    /// Register a tool token directly. Tests drive the MCP route without
    /// starting a harness; sessions always go through `start`. A token only
    /// authorizes a live session, so a session the test never created is
    /// recorded as running here.
    #[doc(hidden)]
    pub async fn register_tool_token_for_test(&self, session_id: &str, channel: &str) -> String {
        if self.store.get_session(session_id).ok().flatten().is_none() {
            let now = crate::store::now_ms();
            self.store
                .ensure_peer_node(&self.node_id)
                .expect("test session node row");
            self.store
                .insert_session(&SessionRow {
                    id: session_id.to_string(),
                    node_id: self.node_id.clone(),
                    channel: channel.to_string(),
                    work_item_id: None,
                    repo_path: String::new(),
                    worktree_path: None,
                    branch: String::new(),
                    harness_id: "fake".into(),
                    harness_version: String::new(),
                    harness_agent: None,
                    harness_found: None,
                    harness_protocol: None,
                    harness_session_id: None,
                    container_name: None,
                    model: String::new(),
                    project_id: None,
                    phase: Phase::Execute.as_str().into(),
                    policy_version: None,
                    review_id: None,
                    budget_tokens: 0,
                    tokens_used: 0,
                    cost_usd: None,
                    context_used: None,
                    context_size: None,
                    state: SessionState::Running.as_str().into(),
                    end_reason: None,
                    last_error: None,
                    turn_active: 0,
                    draft: None,
                    draft_updated_ms: None,
                    created_ms: now,
                    started_mono_ms: Some(0),
                    ended_mono_ms: None,
                    updated_ms: now,
                    archived_ms: None,
                    legacy_ms: None,
                    parent_session: None,
                    continued_from: None,
                    forked_from: None,
                    manifest_digest: None,
                    open_turn_tokens: 0,
                    request_id: None,
                })
                .expect("test session row");
        }
        let token = mint_token();
        self.tokens
            .lock()
            .await
            .insert(session_id.to_string(), (token.clone(), channel.to_string()));
        token
    }

    /// The channel a tool call may act as, or `None` if the token does not
    /// match a live session. Compared in constant time: a token is a secret.
    pub async fn authorize_tool_call(&self, session_id: &str, presented: &str) -> Option<String> {
        let channel = {
            let tokens = self.tokens.lock().await;
            let (expected, channel) = tokens.get(session_id)?;
            constant_time_eq(expected.as_bytes(), presented.as_bytes()).then(|| channel.clone())?
        };
        self.session_allows_work(session_id).then_some(channel)
    }

    /// The session a gateway request belongs to, from the placeholder key it
    /// carries: `(session_id, channel)`.
    /// How many sessions currently hold a capability token, for a refusal to
    /// say whether the node had forgotten this one or never knew any.
    pub async fn live_token_count(&self) -> usize {
        self.tokens.lock().await.len()
    }

    pub async fn session_for_token(&self, presented: &str) -> Option<(String, String)> {
        let found = {
            let tokens = self.tokens.lock().await;
            tokens
                .iter()
                .find(|(_, (expected, _))| {
                    constant_time_eq(expected.as_bytes(), presented.as_bytes())
                })
                .map(|(id, (_, channel))| (id.clone(), channel.clone()))
        };
        let (id, channel) = found?;
        self.session_allows_work(&id).then_some((id, channel))
    }

    /// Reassert a session fence immediately before an irreversible brokered
    /// action. Token validation authorizes a request at ingress; callers that
    /// await policy, checks, or an external service must call this again at
    /// dispatch so a concurrent pause or stop wins.
    pub fn ensure_active(&self, session_id: &str) -> Result<(), SessionError> {
        let row = self
            .store
            .get_session(session_id)?
            .ok_or(SessionError::NotFound)?;
        // Before the state, because "no longer active" is true of a legacy
        // session and useless to the operator: what they need is that its
        // harness is gone and how to carry the work forward.
        Self::refuse_legacy(&row)?;
        let state = SessionState::from_stored(&row.state);
        if state == SessionState::Paused {
            if row.budget_tokens > 0 && row.tokens_used >= row.budget_tokens {
                return Err(SessionError::Rejected(
                    "session is paused at its token budget; raise the budget to carry on".into(),
                ));
            }
            return Err(SessionError::Rejected("session is paused".into()));
        }
        if state.is_terminal() {
            return Err(SessionError::Rejected("session is no longer active".into()));
        }
        Ok(())
    }

    /// Refuse anything that would put a legacy session back to work.
    ///
    /// A session archived by `archive-legacy` is read-only for good: no
    /// adapter resolves its harness, so there is nothing to launch, and a
    /// second process writing under its id would make its transcript
    /// unreadable. The refusal names the reopen path rather than leaving the
    /// operator with a session that merely will not start.
    pub fn refuse_legacy(row: &SessionRow) -> Result<(), SessionError> {
        if row.legacy_ms.is_some() {
            return Err(SessionError::Legacy {
                id: row.id.clone(),
                harness: row.harness_id.clone(),
            });
        }
        Ok(())
    }

    fn session_allows_work(&self, session_id: &str) -> bool {
        self.ensure_active(session_id).is_ok()
    }
    pub fn probe_token(&self) -> &str {
        &self.probe_token
    }

    /// Why the last model probe failed, if it did.
    pub fn probe_failure(&self) -> Option<ProbeFailure> {
        self.probe_failure.read().clone()
    }

    /// Record how a model probe ended: a failure is kept, an answer clears it.
    pub fn set_probe_failure(&self, failure: Option<ProbeFailure>) {
        *self.probe_failure.write() = failure;
    }

    /// Held for one model probe at a time.
    pub async fn probing(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.probing.lock().await
    }

    pub fn is_probe_token(&self, presented: &str) -> bool {
        constant_time_eq(self.probe_token.as_bytes(), presented.as_bytes())
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Take a checked, node-owned snapshot of a durable workspace. Review,
    /// export, and publication consume this path; no caller receives the
    /// mutable runtime volume or an operator-selected source path.
    pub async fn snapshot_workspace(&self, session_id: &str) -> Result<PathBuf, SessionError> {
        let session = self
            .store
            .get_session(session_id)?
            .ok_or(SessionError::NotFound)?;
        if session.node_id != self.node_id {
            return Err(SessionError::Remote(session.node_id, session.channel));
        }
        if session.harness_id == external::HARNESS_ID {
            return session
                .worktree_path
                .map(PathBuf::from)
                .ok_or_else(|| SessionError::Rejected("external session has no worktree".into()));
        }
        let workspace_id = session
            .repo_path
            .strip_prefix("workspace://")
            .unwrap_or(&session.id);
        let workspace = crate::workspace::Workspace {
            id: workspace_id.to_string(),
            volume: crate::workspace::volume_name(workspace_id),
            snapshot: crate::workspace::snapshot_path(workspace_id),
        };
        let snapshot = crate::workspace::export(self.backend.as_ref(), &workspace)
            .await
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        crate::workspace::sanitize_git(&snapshot)
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        self.store.update_session(
            session_id,
            SessionPatch {
                worktree_path: Some(snapshot.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )?;
        Ok(snapshot)
    }

    /// The same snapshot, addressed by either a session id or the id of an
    /// imported workspace that has not run a session yet: an operator can
    /// export and download what they imported without starting anything.
    pub async fn snapshot_workspace_or_session(&self, id: &str) -> Result<PathBuf, SessionError> {
        if self.store.get_session(id)?.is_some() {
            return self.snapshot_workspace(id).await;
        }
        let workspace = crate::workspace::Workspace {
            id: id.to_string(),
            volume: crate::workspace::volume_name(id),
            snapshot: crate::workspace::snapshot_path(id),
        };
        let snapshot = crate::workspace::export(self.backend.as_ref(), &workspace)
            .await
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        crate::workspace::sanitize_git(&snapshot)
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        Ok(snapshot)
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn policy(&self) -> &Arc<parking_lot::RwLock<crate::policy::Policy>> {
        &self.policy
    }

    /// Republish the waiting bay. Called when a review arrives or is decided,
    /// so the queue updates without the operator refetching.
    /// It also lets go of what the node kept in memory for reviews no longer
    /// open, however they were decided: by the operator here, through a
    /// peer, or withdrawn.
    pub async fn publish_queue(&self) {
        let waiting = self.store.open_permission_views().unwrap_or_default();
        self.bus.publish(Frame::Queue { waiting });
        if let Ok(reviews) = self.store.open_reviews() {
            let open: std::collections::HashSet<&str> =
                reviews.iter().map(|r| r.id.as_str()).collect();
            self.review_starts().retain_open(&open);
            self.waits().retain_open(&open);
            self.bus.publish(Frame::Reviews { waiting: reviews });
        }
    }

    /// A review session may read only its owning node's code-review row. This
    /// runs before any project/session/event side effect, so a peer or HTTP
    /// caller cannot use a report ID to inject its narrative into an unrelated
    /// session's orientation.
    fn validate_review_context(&self, spec: &NewSession) -> Result<(), SessionError> {
        let requested = spec.review_id.as_deref().map(str::trim);
        if spec.phase != Phase::Review {
            if requested.is_some() {
                return Err(SessionError::Rejected(
                    "review_id is only valid for a code-review session".into(),
                ));
            }
            return Ok(());
        }
        let Some(id) = requested.filter(|id| !id.is_empty()) else {
            return Err(SessionError::Rejected(
                "a code-review session requires its review_id".into(),
            ));
        };
        let review = self
            .store
            .get_review(id)?
            .ok_or_else(|| SessionError::Rejected(format!("no code review {id}")))?;
        if review.kind == crate::store::reports::KIND {
            return Err(SessionError::Rejected(
                "standalone narrative reports cannot start code-review sessions".into(),
            ));
        }
        if !matches!(review.kind.as_str(), "pr" | "mr") {
            return Err(SessionError::Rejected(format!(
                "review {id} is not a supported code-review kind"
            )));
        }
        if review.channel != spec.channel {
            return Err(SessionError::Rejected(format!(
                "review {id} belongs to channel {}",
                review.channel
            )));
        }
        if review.node_id != self.node_id {
            return Err(SessionError::Rejected(format!(
                "review {id} is owned by node {}",
                review.node_id
            )));
        }
        Ok(())
    }

    /// Validate, insert the row, and start the session in the background. The
    /// row exists before the harness does, so the interface can show a session
    /// that is still starting.
    pub async fn create(&self, spec: NewSession) -> Result<SessionRow, SessionError> {
        self.create_on(spec, None).await
    }

    /// `create`, on an adapter the caller chose. The node's own paths never
    /// choose: the harness follows the model's credential. This is the seam a
    /// test drives a fake harness through.
    pub async fn create_with(
        &self,
        spec: NewSession,
        adapter: Arc<dyn HarnessAdapter>,
    ) -> Result<SessionRow, SessionError> {
        self.create_on(spec, Some(adapter)).await
    }

    async fn create_on(
        &self,
        mut spec: NewSession,
        chosen: Option<Arc<dyn HarnessAdapter>>,
    ) -> Result<SessionRow, SessionError> {
        if let Some(request_id) = spec.request_id.as_deref() {
            check_request_id(request_id)?;
        }
        // Asked to run elsewhere: the owner validates and starts it; its row
        // arrives back both in the ack and, shortly, as a mirrored session.
        if let Some(node) = spec.node_id.as_deref().filter(|n| *n != self.node_id) {
            let mut remote = spec.clone();
            remote.node_id = None;
            // Sent with an id even when the caller gave none, so that what
            // is said when no answer comes can name a safe way to retry.
            let request_id = remote
                .request_id
                .get_or_insert_with(|| uuid::Uuid::now_v7().to_string())
                .clone();
            // Asked again for a create already answered, or reconciled from
            // a late answer: the mirror is the session.
            if let Some(row) = self
                .store
                .session_by_request(node, &remote.channel, &request_id)?
            {
                return Ok(row);
            }
            // The peer resolves the repository on its own disk, so it is sent
            // the forge identity, never this node's path to its clone.
            if remote.workspace_id.is_none() {
                remote.repo_path =
                    crate::forge::forwarded_repo(&Config::state_dir(), remote.repo_path.trim());
            }
            let work_item = match remote.work_item_id.as_deref() {
                Some(id) => self
                    .store
                    .work_item_change(&self.node_id, &remote.channel, id)?,
                None => None,
            };
            let channel = remote.channel.clone();
            let sent = self
                .forward(
                    node,
                    proto::frame::Command::Create {
                        spec: json!(remote),
                        work_item,
                    },
                    false,
                )
                .await;
            let v = match sent {
                Ok(v) => v,
                // The owner may have made it, and its row may yet arrive:
                // as a mirrored session, or as the late answer itself.
                Err(SessionError::OutcomeUnknown { node, .. }) => {
                    let deadline = Instant::now() + LATE_CREATE_GRACE;
                    loop {
                        if let Some(row) =
                            self.store
                                .session_by_request(&node, &channel, &request_id)?
                        {
                            return Ok(row);
                        }
                        if Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    return Err(SessionError::OutcomeUnknown {
                        node,
                        request_id: Some(request_id),
                    });
                }
                Err(e) => return Err(e),
            };
            let row: SessionRow =
                serde_json::from_value(v).map_err(|e| SessionError::Rejected(e.to_string()))?;
            let _ = self.store.ensure_peer_node(node);
            self.store.upsert_session_mirror(&row)?;
            self.bus
                .publish_untapped(Frame::Session(Box::new(row.clone())));
            return Ok(row);
        }
        // A create sent again with the id of one already made, here: the
        // session that one made. Two with the same id at once are taken one
        // after the other, so the second finds the first's row.
        let _same_request = match spec.request_id.clone() {
            Some(request_id) => {
                let guard = self.requests.hold(&spec.channel, &request_id).await;
                if let Some(row) =
                    self.store
                        .session_by_request(&self.node_id, &spec.channel, &request_id)?
                {
                    return Ok(row);
                }
                Some(guard)
            }
            None => None,
        };
        self.refuse_while_restarting()?;
        self.validate_review_context(&spec)?;
        // A dangling ancestor is worse than none: it reads as history that
        // can be followed and cannot.
        for ancestor in [
            spec.parent_session.as_deref(),
            spec.continued_from.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if self.store.get_session(ancestor)?.is_none() {
                return Err(SessionError::Rejected(format!(
                    "no session {ancestor} on this node to continue from"
                )));
            }
        }
        let bindings = self.bindings(&spec.channel);
        // An archived channel keeps everything it has and takes nothing new.
        if !bindings["archived"].is_null() {
            return Err(SessionError::ChannelArchived(spec.channel.clone()));
        }
        let phase_bindings = &bindings["phases"][spec.phase.as_str()];
        let on_exhaustion = exhaustion::resolve(spec.on_exhaustion.as_ref(), &bindings)
            .map_err(SessionError::Rejected)?;
        let (model, model_source) = self.resolve_model(&spec, &bindings)?;
        spec.model = model;
        let budget = spec
            .budget_tokens
            .or_else(|| phase_bindings["budget_tokens"].as_i64())
            .unwrap_or(self.cfg.live().session.budget_tokens);
        if budget < 0 {
            return Err(SessionError::BadBudget);
        }
        // Runaway spend is the failure mode that scales with how well the
        // system works: a channel at its daily ceiling starts nothing.
        let ceiling = crate::metrics::ceiling(&self.store, &bindings, &spec.channel);
        if ceiling.at() {
            return Err(SessionError::Ceiling(ceiling.reason()));
        }
        // A meshed node seals every frame under the channel's key, so a channel
        // it has no keyring for cannot carry a session. A standalone node keeps
        // channels as plain labels.
        if self.cfg.mesh.hub_url.is_some() && self.store.channel_get(&spec.channel)?.is_none() {
            return Err(SessionError::UnknownChannel(spec.channel.clone()));
        }
        // The harness follows the model's credential, once everything that
        // would refuse the session whatever it ran on has had its say.
        let (adapter, harness_source) = match chosen {
            Some(adapter) => {
                let class = self.credential_class(&spec.model, Some(&spec.channel));
                self.check_fit(&spec.model, class, adapter.id())?;
                (adapter, "caller")
            }
            None => self.resolve_harness(&spec)?,
        };
        if let Some(node) = self.store.get_node(&self.node_id)? {
            // Where DigitalOcean may take sessions, a node whose own boundary
            // is not verified asks the operator to run one there instead
            // (`placing`), so the session is made.
            if node.state != "ready" && !crate::provider::digitalocean::runtime::enabled(&self.cfg)
            {
                return Err(SessionError::NodeRefused(
                    node.failed_detail
                        .or(node.failed_check)
                        .unwrap_or_else(|| "boundary check failed".into()),
                ));
            }
            // Each harness's probe is on the node row; a harness it does not
            // list is checked at its own handshake.
            if let Some(harness) = node.harnesses().into_iter().find(|h| h.id == adapter.id()) {
                if harness.mismatch() {
                    return Err(SessionError::VersionMismatch {
                        found: harness.found.unwrap_or_default(),
                        pinned: harness.pinned,
                    });
                }
            }
        }

        // Work items and phase artifacts are an opt-in workflow. Plain
        // workspace sessions use the execute harness mode without claiming an
        // item, plan, or review lifecycle.
        let mut held_item = None;
        if let Some(item_id) = spec
            .work_item_id
            .as_deref()
            .filter(|i| !i.trim().is_empty())
        {
            let view = self
                .store
                .work_status(&spec.channel, None)?
                .into_iter()
                .find(|v| v.item.id == item_id)
                .ok_or_else(|| {
                    SessionError::NotReady(format!("no task {item_id} on {}", spec.channel))
                })?;
            match &view.readiness {
                tracon_sync::work::Readiness::Ready => {}
                tracon_sync::work::Readiness::Closed => {
                    return Err(SessionError::NotReady("it is closed".into()))
                }
                tracon_sync::work::Readiness::Blocked { by } => {
                    let by: Vec<String> = by
                        .iter()
                        .map(|b| match b {
                            tracon_sync::work::Blocker::Open { id } => {
                                format!("open {}", crate::names::task(id))
                            }
                            tracon_sync::work::Blocker::Unknown { id } => {
                                format!("{} not seen here", crate::names::task(id))
                            }
                            tracon_sync::work::Blocker::Cycle => "a dependency cycle".into(),
                        })
                        .collect();
                    return Err(SessionError::NotReady(format!(
                        "blocked by {}",
                        by.join(", ")
                    )));
                }
            }
            // A suspended session still holds its item; the session carrying
            // it on takes the item over as the suspended one ends. A reviewer
            // reads the item it reviews against and takes nothing: the session
            // that submitted the change still holds it.
            if let Some(holder) = view.session_id.filter(|holder| {
                spec.phase != Phase::Review
                    && spec.continued_from.as_deref() != Some(holder.as_str())
            }) {
                return Err(SessionError::InSession(holder));
            }
            let requires_plan = bindings["phases"]["execute"]["requires_plan"]
                .as_bool()
                .unwrap_or(true);
            if spec.phase == Phase::Execute && requires_plan {
                let planned = view
                    .item
                    .phase_plan_slug
                    .as_deref()
                    .and_then(|slug| self.store.doc_get(&spec.channel, slug).ok().flatten())
                    .is_some();
                if !planned {
                    return Err(SessionError::PlanRequired(item_id.to_string()));
                }
            }
            held_item = Some(view.item);
        } else if spec.phase == Phase::Plan {
            return Err(SessionError::WorkItemRequired);
        }
        // Starting work is one action. A session the node starts on an item
        // or a review is sent its kickoff as the first prompt, rather than
        // sitting `running` until the operator thinks to type something. A
        // caller's own first prompt (a plain session, a reopen's handoff)
        // wins; the kickoff is not kept as a draft, because it is the node's
        // words and not the operator's.
        let kickoff = match spec.initial_prompt.as_deref() {
            Some(prompt) if !prompt.trim().is_empty() => None,
            _ => kickoff(spec.phase, held_item.as_ref(), spec.review_id.as_deref()),
        };
        // A new workspace starts from the node's own clone of a forge
        // repository, named by its identity, at the forge's default branch or
        // the forge branch it names. Never an operator's checkout, and never
        // a commit of the caller's choosing: a review session's base is the
        // node's, and it arrives with its workspace already made.
        if spec.workspace_id.is_none() {
            let repo = crate::forge::session_repo(&Config::state_dir(), spec.repo_path.trim())
                .map_err(SessionError::Source)?;
            spec.repo_path = repo.to_string_lossy().into_owned();
            if spec.base_sha.is_some() {
                return Err(SessionError::Source(
                    "a session starts from its forge's default branch, or the forge branch it \
                     names, not from a commit"
                        .into(),
                ));
            }
        }
        // Bank identity: the channel and the repository's remote, resolved on
        // this side of the boundary and recorded on the row for memory to key
        // on; never a checkout path.
        let carried_project = spec
            .project_id
            .as_deref()
            .map(|id| self.store.project_get(id))
            .transpose()?
            .flatten()
            .filter(|project| project.channel == spec.channel);
        let (project_id, project_name, remote) = match spec.workspace_id.as_deref() {
            // Carried on from another session (a move): the same project,
            // as this node has it.
            _ if carried_project.is_some() => {
                let project = carried_project.expect("checked above");
                (project.id, project.name, project.remote_url)
            }
            Some(workspace_id) => {
                let canonical = format!("workspace/{workspace_id}");
                (
                    crate::corpus::project::project_id(&spec.channel, &canonical),
                    "imported workspace".to_string(),
                    None,
                )
            }
            // Not cloned yet: the identity it was named by is the remote the
            // clone will have.
            None if !std::path::Path::new(&spec.repo_path).join(".git").exists() => {
                let identity = crate::forge::managed_identity(
                    &Config::state_dir(),
                    std::path::Path::new(&spec.repo_path),
                )
                .unwrap_or_default();
                let name = identity.rsplit('/').next().unwrap_or("repo").to_string();
                (
                    crate::corpus::project::project_id(&spec.channel, &identity),
                    name,
                    Some(identity),
                )
            }
            None => {
                crate::corpus::project::identify(
                    &spec.channel,
                    std::path::Path::new(&spec.repo_path),
                    &self.cfg.live().publish.git,
                )
                .await
            }
        };
        let _ = self.store.project_put(&crate::store::ProjectRow {
            id: project_id.clone(),
            channel: spec.channel.clone(),
            name: project_name,
            remote_url: remote,
            created_ms: now_ms(),
        });
        // An item with no project takes the one its first session runs in,
        // so a composed task is grouped with its repository from then on.
        if let Some(item) = held_item.as_ref().filter(|i| i.project_id.is_none()) {
            let _ = crate::corpus::work::update(
                &self.store,
                &self.bus,
                &self.node_id,
                &item.id,
                crate::corpus::work::Patch {
                    project_id: Some(Some(project_id.clone())),
                    ..Default::default()
                },
                None,
            );
        }
        let id = uuid::Uuid::now_v7().to_string();
        let slug = id.split('-').next_back().unwrap_or("session").to_string();
        let branch = spec
            .branch
            .clone()
            .unwrap_or_else(|| format!("feat/tracon-{slug}"));
        let row = SessionRow {
            id: id.clone(),
            node_id: self.node_id.clone(),
            channel: spec.channel.clone(),
            work_item_id: spec.work_item_id.clone(),
            repo_path: spec
                .workspace_id
                .as_deref()
                .map(|workspace_id| format!("workspace://{workspace_id}"))
                .unwrap_or_else(|| spec.repo_path.clone()),
            worktree_path: None,
            branch: branch.clone(),
            harness_id: adapter.id().to_string(),
            harness_version: adapter.pinned_version().to_string(),
            harness_agent: None,
            harness_found: None,
            harness_protocol: None,
            harness_session_id: None,
            container_name: None,
            model: spec.model.clone(),
            project_id: Some(project_id),
            phase: spec.phase.as_str().into(),
            policy_version: Some(self.policy.read().version as i64),
            review_id: spec.review_id.clone(),
            budget_tokens: budget,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: SessionState::Starting.as_str().into(),
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: now_ms(),
            started_mono_ms: Some(0),
            ended_mono_ms: None,
            updated_ms: now_ms(),
            archived_ms: None,
            legacy_ms: None,
            parent_session: spec.parent_session.clone(),
            continued_from: spec.continued_from.clone(),
            forked_from: spec.forked_from.clone(),
            manifest_digest: None,
            open_turn_tokens: 0,
            request_id: spec.request_id.clone(),
        };
        self.store.insert_session(&row)?;
        self.store.exhaustion_choose(&id, &on_exhaustion)?;
        // Reviews follow a continuation on the node they are on. One from
        // another node (a moved session) leaves its reviews where they are:
        // the new session resubmits from the branch it was given.
        if let Some(from) = spec.continued_from.as_deref().filter(|from| {
            self.store
                .get_session(from)
                .ok()
                .flatten()
                .is_some_and(|row| row.node_id == self.node_id)
        }) {
            self.take_over_reviews(from, &id, &spec.told_reviews)
                .await?;
        }
        // A plain session has no work item to retain its first instruction.
        // Keep it as a draft before asynchronous setup, and let on_prompt
        // clear it only after the supervisor accepts it.
        if let Some(prompt) = spec
            .initial_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            self.store.set_draft(&id, Some(prompt))?;
        }
        let from_node = kickoff.is_some();
        if from_node {
            spec.initial_prompt = kickoff;
        }
        let row = self.store.get_session(&id)?.ok_or(SessionError::NotFound)?;
        self.bus.publish(Frame::Session(Box::new(row.clone())));

        let this = self.clone();
        let started = Instant::now();
        let series = session_series(&self.store, &id, &spec, adapter.id());
        tokio::spawn(async move {
            if let Err(e) = this
                .start(
                    &id,
                    spec,
                    branch,
                    slug,
                    adapter,
                    started,
                    (model_source, harness_source),
                    from_node,
                    None,
                )
                .await
            {
                tracing::error!(session = %id, error = %e, "session failed to start");
                // Nothing is writing this session's state now, whatever the
                // launch got as far as: the fence is given back here, because
                // the supervisor that would have released it never ran.
                materialize::release_state(&id);
                // A stop can race startup before a supervisor exists. It is
                // terminal by the time startup reports its cancellation and
                // must not be overwritten as a failed launch.
                let terminal = this
                    .store
                    .get_session(&id)
                    .ok()
                    .flatten()
                    .is_some_and(|row| SessionState::from_stored(&row.state).is_terminal());
                if terminal {
                    this.tokens.lock().await.remove(&id);
                    return;
                }
                // A session that never started holds no capability.
                this.tokens.lock().await.remove(&id);
                crate::telemetry::telemetry().session_start_failed(&series);
                // A harness that is not the one this node drives is a fact
                // about the image, not about this session: every session here
                // ends the same way until the image or the pin changes, and
                // the row says so rather than reading as one bad start.
                let reason = match e.downcast_ref::<crate::adapter::AdapterError>() {
                    Some(
                        crate::adapter::AdapterError::IncompatibleProtocol { .. }
                        | crate::adapter::AdapterError::VersionMismatch { .. },
                    ) => EndReason::Incompatible,
                    _ => EndReason::Error,
                };
                let _ = this.store.update_session(
                    &id,
                    SessionPatch {
                        state: Some(SessionState::Failed.as_str().into()),
                        end_reason: Some(reason.as_str().into()),
                        last_error: Some(e.to_string()),
                        ended_mono_ms: Some(started.elapsed().as_millis() as i64),
                        ..Default::default()
                    },
                );
                // Recorded through the bus, not just the store: a client
                // watching (here or on a peer) should see why it never started.
                this.record(NewEvent {
                    session_id: id.clone(),
                    work_item_id: None,
                    kind: ek::ERROR.into(),
                    ref_id: None,
                    payload: json!({ "error": e.to_string() }),
                    at_ms: now_ms(),
                    mono_ms: started.elapsed().as_millis() as i64,
                });
                if let Ok(Some(row)) = this.store.get_session(&id) {
                    this.bus.publish(Frame::Session(Box::new(row)));
                }
            }
        });
        Ok(row)
    }

    /// [`Self::record`], for what the node learns about a session from
    /// outside it: a published request moving on its forge.
    pub fn record_new_event(&self, e: NewEvent) {
        self.record(e)
    }

    /// Persist a lifecycle event and publish it on the stream, so a client
    /// watching live sees the same log a reload rebuilds.
    fn record(&self, e: NewEvent) {
        match self.store.append_event(&e) {
            Ok(seq) => self.bus.publish(Frame::Event {
                seq,
                node_id: self.node_id.clone(),
                session_id: e.session_id,
                kind: e.kind,
                ref_id: e.ref_id,
                payload: e.payload,
                at_ms: e.at_ms,
            }),
            Err(err) => tracing::error!(error = %err, "failed to persist event"),
        }
    }

    fn startable(&self, id: &str) -> Result<bool, crate::store::StoreError> {
        Ok(self
            .store
            .get_session(id)?
            .is_some_and(|row| row.state == SessionState::Starting.as_str()))
    }

    /// Sessions running on this node's own runtime now, or placed there and
    /// still starting, besides `id`.
    async fn running_locally(&self, id: &str) -> u32 {
        let live: Vec<String> = self.live.lock().await.keys().cloned().collect();
        self.slots.running_locally(&live, id)
    }

    /// Decide where a starting session runs (`placing`), asking the
    /// operator first when that is what the decision is, and record it.
    /// Placed here, the session holds a slot of `local_limit` until its
    /// start returns; the count and the slot are taken under one lock, so
    /// starts at the same moment do not all see the same free slot.
    /// `unschedulable` is the scheduler's refusal of the session's pod on
    /// this node's own runtime, which then counts as full: the session is
    /// placed again, and the decision recorded, even where placement is
    /// otherwise not in play.
    async fn place(
        &self,
        id: &str,
        channel: &str,
        origin: Option<&std::path::Path>,
        started: Instant,
        unschedulable: Option<&str>,
    ) -> anyhow::Result<placing::Placed> {
        use crate::provider::digitalocean::runtime as do_runtime;
        use crate::provider::{Placement, ProviderKind, Room};
        let resolved = placing::resolve(&self.cfg, channel, origin);
        let runtime = do_runtime::global().filter(|_| do_runtime::enabled(&self.cfg));
        if unschedulable.is_none() && !placing::in_play(runtime.is_some(), &resolved) {
            self.slots.local(id);
            return Ok(placing::Placed::Local);
        }
        let deciding = self.slots.deciding().await;
        let mut providers = Vec::new();
        if let Some(kind) = ProviderKind::of_backend(self.backend.kind()) {
            let node = self.store.get_node(&self.node_id).ok().flatten();
            let mut local = placing::local_candidate(
                kind,
                placing::node_health(node.as_ref()),
                self.running_locally(id).await,
                self.cfg.live().placement.local_limit,
            );
            local.room = match unschedulable {
                Some(said) => Some(Room::unschedulable(said)),
                None => self.backend.room().await.map(|read| match read {
                    Ok(fit) => Room::Read(fit),
                    Err(unread) => Room::Unread { unread },
                }),
            };
            providers.push(local);
        }
        if let Some(runtime) = &runtime {
            providers.push(runtime.candidate(&self.cfg).await);
        }
        let mode = self.cfg.live().placement.digitalocean;
        let decision = crate::provider::place(
            &providers,
            &resolved.requirements,
            mode,
            resolved.forbid_digitalocean,
        );
        let record = |payload: serde_json::Value| {
            self.record(NewEvent {
                session_id: id.to_string(),
                work_item_id: None,
                kind: ek::PLACEMENT.into(),
                ref_id: None,
                payload,
                at_ms: now_ms(),
                mono_ms: started.elapsed().as_millis() as i64,
            })
        };
        record(placing::event(&decision, &resolved, &providers));
        let elsewhere =
            |runtime: Option<Arc<crate::provider::digitalocean::runtime::DoRuntime>>| {
                let runtime = runtime.ok_or_else(|| {
                    anyhow::anyhow!("DigitalOcean was chosen but is not running on this node")
                })?;
                self.slots.elsewhere(id);
                Ok::<_, anyhow::Error>(placing::Placed::DigitalOcean {
                    runtime,
                    requirements: resolved.requirements.clone(),
                })
            };
        match decision {
            Placement::Place {
                provider: ProviderKind::DigitalOcean,
            } => elsewhere(runtime),
            Placement::Place { .. } => {
                self.slots.local(id);
                Ok(placing::Placed::Local)
            }
            Placement::Refuse { reason } => {
                anyhow::bail!("no sandbox provider can take this session: {reason}")
            }
            Placement::Ask { reason, offer } => {
                // The operator may take minutes: other starts decide meanwhile.
                drop(deciding);
                let card = placing::ask(
                    &self.store,
                    &self.node_id,
                    channel,
                    id,
                    &reason,
                    offer,
                    self.cfg.live().session.approval_expiry_secs,
                )?;
                self.publish_queue().await;
                loop {
                    match placing::answer(&self.store, &card.id)? {
                        placing::Answer::Waiting => {}
                        placing::Answer::Allowed => {
                            record(serde_json::json!({
                                "decision": "place",
                                "provider": offer,
                                "approval_id": card.id,
                            }));
                            return elsewhere(runtime);
                        }
                        placing::Answer::Declined(why) => {
                            anyhow::bail!("not run on {}: {why}", offer.as_str())
                        }
                    }
                    if !self.startable(id)? {
                        anyhow::bail!("the session was stopped while its placement was asked");
                    }
                    tokio::time::sleep(placing::ASK_POLL).await;
                }
            }
        }
    }

    /// Decide where a review's required checks run (`placing::place_checks`):
    /// on the node's own runtime, counted short of room when `short` says
    /// why, or on DigitalOcean. Recorded on `session_id`, when there is one,
    /// as a `placement` event `for` checks. Out of play, with nothing
    /// recorded, where DigitalOcean is not on.
    pub async fn place_checks(
        &self,
        session_id: Option<&str>,
        channel: &str,
        origin: Option<&std::path::Path>,
        short: Option<&str>,
    ) -> placing::ChecksPlaced {
        use crate::provider::digitalocean::runtime as do_runtime;
        use crate::provider::ProviderKind;
        let Some(runtime) = do_runtime::global().filter(|_| do_runtime::enabled(&self.cfg)) else {
            return placing::ChecksPlaced::Local;
        };
        let resolved = placing::resolve(&self.cfg, channel, origin);
        let mut providers = Vec::new();
        if let Some(kind) = ProviderKind::of_backend(self.backend.kind()) {
            let node = self.store.get_node(&self.node_id).ok().flatten();
            providers.push(placing::checks_candidate(
                kind,
                placing::node_health(node.as_ref()),
                short,
            ));
        }
        providers.push(runtime.candidate(&self.cfg).await);
        let decision = placing::place_checks(
            &providers,
            &resolved,
            self.cfg.live().placement.digitalocean,
        );
        if let Some(id) = session_id {
            let mut payload = placing::event(&decision, &resolved, &providers);
            payload["for"] = serde_json::json!("checks");
            payload["short"] = serde_json::json!(short);
            self.record_event(id, ek::PLACEMENT, payload);
        }
        match placing::checks_elsewhere(&decision) {
            true => placing::ChecksPlaced::DigitalOcean {
                runtime,
                requirements: resolved.requirements,
            },
            false => placing::ChecksPlaced::Local,
        }
    }

    /// Prepare a starting session's workspace (`prepare_session_workspace`)
    /// and record how it went. `None` when the entry prepares nothing.
    #[allow(clippy::too_many_arguments)]
    async fn prepare_workspace(
        &self,
        id: &str,
        slug: &str,
        workspace: &crate::workspace::Workspace,
        cache: Result<&str, String>,
        environment: &crate::environment::RepoEnvironment,
        from: Option<FromSnapshot<'_>>,
        started: Instant,
    ) -> Option<crate::environment::WorkspacePreparation> {
        // Carried on in the sandbox the node's previous process left running
        // (`runner::reattach`): the workspace there is the one the session
        // had, prepared, and newer than the node's copy, which is all a
        // preparation here would reach.
        let carried_on =
            crate::runner::reattach::is_kept(&crate::config::resource_name(&format!("h-{slug}")));
        let preparation = match carried_on {
            true if !environment.prepare.is_empty() => crate::environment::WorkspacePreparation {
                commands: environment.prepare.clone(),
                outcome: crate::environment::PrepareOutcome::NotRun(
                    "the session carries on in its sandbox, whose workspace is as it was".into(),
                ),
                elapsed_ms: 0,
            },
            _ => {
                prepare_session_workspace(
                    self.backend.as_ref(),
                    environment,
                    (&workspace.volume, cache),
                    &crate::config::resource_name(&format!("s-{slug}")),
                    &Stopping {
                        store: &self.store,
                        id,
                    },
                    from,
                )
                .await?
            }
        };
        if !carried_on
            && !matches!(
                preparation.outcome,
                crate::environment::PrepareOutcome::Prepared
                    | crate::environment::PrepareOutcome::FromSnapshot { .. }
            )
        {
            tracing::warn!(
                session = %id,
                outcome = preparation.outcome.as_str(),
                "the session's workspace was not prepared"
            );
        }
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::WORKSPACE_PREPARED.into(),
            ref_id: None,
            payload: preparation.payload(),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });
        Some(preparation)
    }

    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        id: &str,
        spec: NewSession,
        branch: String,
        slug: String,
        adapter: Arc<dyn HarnessAdapter>,
        started: Instant,
        (model_source, harness_source): (&'static str, &'static str),
        kickoff: bool,
        reopened: Option<Reopened>,
    ) -> anyhow::Result<()> {
        // Until this returns, the session's slot is held if it is placed
        // here; by then it is live, or it never will be.
        let _starting = self.slots.starting(id);
        if !self.startable(id)? {
            return Ok(());
        }
        // Registered before the harness starts: it connects to the node's MCP
        // server during `session/new`, so a token published afterwards is
        // published too late and the node refuses its own harness.
        let token = mint_token();
        self.tokens
            .lock()
            .await
            .insert(id.to_string(), (token.clone(), spec.channel.clone()));

        let repo = PathBuf::from(&spec.repo_path);
        let git = self.cfg.live().publish.git.clone();
        let mut synced = None;
        let (workspace, base) = match spec.workspace_id.as_deref() {
            Some(existing) => (
                crate::workspace::Workspace {
                    id: existing.to_string(),
                    volume: crate::workspace::volume_name(existing),
                    snapshot: crate::workspace::snapshot_path(existing),
                },
                None,
            ),
            None => {
                // `create` resolved the repository to its managed clone. One
                // not on this node yet is cloned now, with the channel's
                // forge credential: minutes for a large repository, once,
                // so it is said before it is waited on.
                if !repo.join(".git").exists() {
                    let identity = crate::forge::managed_identity(&Config::state_dir(), &repo)
                        .ok_or_else(|| anyhow::anyhow!(crate::forge::HOST_CHECKOUT_REFUSED))?;
                    self.record(NewEvent {
                        session_id: id.to_string(),
                        work_item_id: None,
                        kind: ek::CLONING.into(),
                        ref_id: None,
                        payload: json!({ "repo": identity }),
                        at_ms: now_ms(),
                        mono_ms: started.elapsed().as_millis() as i64,
                    });
                    crate::forge::ensure_clone_of(
                        &self.tools.broker,
                        &spec.channel,
                        &self.node_id,
                        &identity,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{identity} could not be cloned: {e}"))?;
                }
                // Where the path really leads decides: one named under the
                // root that resolves outside it is refused here.
                let repo = crate::forge::managed_clone(&repo).map_err(anyhow::Error::msg)?;
                let brokered = crate::forge::git_token_for(
                    &self.tools.broker,
                    &Config::state_dir(),
                    &spec.channel,
                    &repo,
                    &self.node_id,
                );
                let credential = match &brokered {
                    Some((forge, token)) => {
                        crate::git_remote::brokered(forge.git_user(), Some(token.as_str()))
                    }
                    None => crate::git_remote::Credential::Anonymous,
                };
                let workspace = {
                    // Held from the fetch until the seed has read the tree,
                    // so another session or a build cannot move it between.
                    let lock = crate::forge::clone_lock(&repo);
                    let _guard = lock.lock().await;
                    let sync = crate::forge::sync_managed(
                        &repo,
                        &credential,
                        spec.branch.as_deref(),
                        spec.base.as_deref(),
                    )
                    .await
                    .map_err(anyhow::Error::msg)?;
                    if let Some(error) = &sync.stale {
                        tracing::warn!(
                            session = id, repo = %repo.display(), %error,
                            "managed clone not fetched; starting from its last-fetched branch"
                        );
                    }
                    let start = sync.source_ref.clone();
                    let onto_base = sync.onto_base;
                    let empty = sync.empty.then(|| sync.default.clone());
                    synced = Some(sync);
                    match empty {
                        // An empty repository: nothing to start from, so
                        // the session starts on an orphan branch whose first
                        // commit the review can publish as the repository's.
                        Some(default) => {
                            crate::workspace::seed_empty(&git, &branch, &default, id).await?
                        }
                        None => {
                            crate::workspace::seed_from_clone(
                                &git,
                                &repo,
                                &branch,
                                start.as_deref(),
                                onto_base,
                                id,
                            )
                            .await?
                        }
                    }
                };
                let stage = crate::workspace::staging_path(id);
                // The workspace's read-only `origin`, through the gateway
                // (`gateway::git`), in its own configuration, where the
                // node's rewrites of it keep it.
                if let Some(url) = crate::gateway::git::origin_for(
                    &Config::state_dir(),
                    &repo,
                    &self.backend.harness_host(),
                    self.cfg.gateway.forward_port,
                    &spec.channel,
                ) {
                    crate::gateway::git::set_workspace_origin(&stage, &url)?;
                }
                let base = match synced.as_ref().is_some_and(|sync| sync.empty) {
                    true => None,
                    false => Some(crate::workspace::head_of(&git, &stage).await?),
                };
                crate::workspace::import(self.backend.as_ref(), &workspace, &stage).await?;
                (workspace, base)
            }
        };
        let snapshot = crate::workspace::export(self.backend.as_ref(), &workspace).await?;
        crate::workspace::sanitize_git(&snapshot)?;
        // The origin the workspace names, if it is the gateway's: a resumed
        // workspace keeps the one it was seeded with.
        let workspace_origin = std::fs::read_to_string(snapshot.join(".git").join("config"))
            .ok()
            .and_then(|text| crate::gateway::git::kept_origin(&text));
        if let Some(entry) = adapter
            .refused_workspace_entries()
            .iter()
            .find(|entry| std::fs::symlink_metadata(snapshot.join(entry)).is_ok())
        {
            anyhow::bail!(
                "the workspace carries `{entry}`. The {} harness would load it as its own \
                 configuration after the node's, where it could allow tools without asking or \
                 run commands and plugins, so the session was not started. Remove or rename it \
                 on the branch this work starts from.",
                adapter.id()
            );
        }
        self.store.update_session(
            id,
            SessionPatch {
                worktree_path: Some(snapshot.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )?;
        let stale = synced.as_ref().and_then(|sync| {
            sync.stale
                .as_ref()
                .map(|error| stale_note(error, sync.fetched_ms))
        });
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::WORKTREE.into(),
            ref_id: None,
            payload: json!({
                "workspace_id": workspace.id,
                "volume": workspace.volume,
                "branch": branch,
                "source": match (&spec.workspace_id, &synced) {
                    (Some(_), _) => "resumed",
                    (None, Some(sync)) if sync.empty => "empty repository",
                    (None, _) => "imported",
                },
                "base": base,
                "synced": synced.as_ref().map(|sync| json!({
                    "source_ref": (!sync.empty).then(|| format!("origin/{}", sync.source)),
                    "empty": sync.empty,
                    "source_commit": sync.commit,
                    "fetched_ms": sync.fetched_ms,
                    "stale": stale,
                })),
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        if !self.startable(id)? {
            self.tokens.lock().await.remove(id);
            return Ok(());
        }

        // The harness reaches its models through the node: the providers this
        // channel could actually spend on are wired to the gateway with this
        // session's token as the placeholder key, so the only secret in the
        // container names the session. A provider the gateway would refuse is
        // left out rather than offered and then denied mid-turn. The
        // declared models, and which providers the file still names, are
        // read as they are now: what the picker offered is what is wired.
        let wiring = {
            let bindings = self.bindings(&spec.channel);
            let broker = self.tools.broker.read().unwrap();
            crate::gateway::model::harness_wiring(
                &self.cfg.live(),
                &self.backend.harness_host(),
                &token,
                |name, provider| {
                    let bound = bindings["providers"].as_array().is_none_or(|allowed| {
                        allowed.iter().any(|value| value.as_str() == Some(name))
                    });
                    // A credential this harness may not run on (a Codex
                    // sign-in under Claude Code) is not offered at all.
                    let fits = broker
                        .model_credential(name, &provider.credential, &spec.channel, &self.node_id)
                        .map(|(_, credential)| credential)
                        .or_else(|| broker.get(&provider.credential))
                        .is_none_or(|credential| {
                            crate::adapter::compat::CredentialClass::classify(
                                &provider.shape,
                                &credential.kind,
                            )
                            .fits(adapter.id())
                            .is_ok()
                        });
                    bound
                        && fits
                        && broker
                            .inject_for_provider(
                                name,
                                &provider.credential,
                                &spec.channel,
                                &self.node_id,
                                &provider.shape,
                            )
                            .is_ok()
                },
            )
        };
        // The repository's own image (or its prepared snapshot), so the
        // session has the tools its checks will run with. Resolved from the
        // repository the work came from, which a resumed session's
        // `workspace://` path only leads back to.
        let origin = crate::environment::origin_repo(&self.store, Some(id))
            .ok()
            .flatten();
        let placed = self
            .place(id, &spec.channel, origin.as_deref(), started, None)
            .await?;
        // Carried on in the sandbox the node's previous process left running
        // (`runner::reattach`): the session runs in what that sandbox runs,
        // as its start chose it then. Chosen again it could be another image
        // (a snapshot not found before the base is pinned), which the session
        // would report and its probes would run in.
        let kept = match placed.is_elsewhere() {
            false => crate::runner::reattach::kept_image(&crate::config::resource_name(&format!(
                "h-{slug}"
            ))),
            true => None,
        };
        let image = match origin.clone() {
            _ if kept.is_some() => kept.unwrap_or_default(),
            // The repository's image is this node's runtime's; elsewhere the
            // session runs in the provider's own.
            Some(_) if placed.is_elsewhere() => crate::repo_image::SessionImage {
                note: Some("the session runs on DigitalOcean, in its template's image".into()),
                ..Default::default()
            },
            Some(origin) => {
                let announce = || {
                    self.record(NewEvent {
                        session_id: id.to_string(),
                        work_item_id: None,
                        kind: ek::REPO_IMAGE.into(),
                        ref_id: None,
                        payload: json!({ "status": "building", "repo": origin }),
                        at_ms: now_ms(),
                        mono_ms: started.elapsed().as_millis() as i64,
                    })
                };
                crate::repo_image::session_image(
                    &self.backend,
                    &self.cfg,
                    &self.store,
                    &origin,
                    &announce,
                )
                .await
                .map_err(anyhow::Error::msg)?
            }
            None => Default::default(),
        };
        // A first build takes minutes, and the operator may have stopped the
        // session while it ran.
        if !self.startable(id)? {
            anyhow::bail!("the session was stopped before its harness started");
        }
        // The operator's customization for this channel, resolved against the
        // provider set the wiring just settled and the policy the gate is
        // running. Built and recorded here, before anything is staged: a
        // manifest that would be refused — a duplicate skill name, a plugin
        // the image did not bake — fails the launch rather than producing a
        // session whose configuration is not the one anybody described.
        //
        // Recorded, not just built: the revision this returns is the one this
        // session ran, and the next edit mints a different one that leaves
        // this session alone.
        // The session's own way out, where the runtime issues one, for
        // either harness. It replaces the harness proxy and carries the hosts
        // that proxy allows (`[gateway] allow_hosts`), so nothing the harness
        // reached before is lost; the model and the node's MCP server are on
        // the gateway host, which `NO_PROXY` keeps off any proxy. A harness's
        // shell inherits its environment, so the agent's installs go through
        // the grant.
        // Upstreams that opt sessions in are attached by the grant, so the
        // agent reaches them as the credential's owner without holding it.
        let mut environment = crate::environment::session_environment(&self.cfg, &self.store, id);
        // Its workspace is prepared where the session runs: on the stock
        // base, the repository's prepared snapshot.
        if let Some(image) = &image.image {
            environment.image = image.clone();
        }
        let upstreams = self
            .tools
            .broker
            .read()
            .map(|broker| {
                broker.session_attachments(&self.cfg.live().upstreams, &spec.channel, &self.node_id)
            })
            .unwrap_or_default();
        let egress = self
            .backend
            .egress_grant(crate::environment::session_grant(
                &self.cfg,
                &environment,
                id,
                upstreams,
            ))
            .map_err(|error| {
                tracing::info!(session = %id, %error, "the session keeps the harness proxy");
            })
            .ok();

        // The repository's session cache: what its sessions fetched and
        // compiled, kept for the next one and read by no check. It starts as
        // a copy of the repository's base cache, so a first build does not
        // begin by fetching what the default branch already needs. Only a
        // session in its repository's image has one.
        let session_cache = match &image.image {
            Some(_) => {
                let cache = crate::environment::session_cache_volume(
                    &environment.cache_volume,
                    &spec.channel,
                );
                let seeded = self
                    .backend
                    .seeds_cache(&environment.cache_volume, image.from_snapshot(&self.cfg))
                    .await;
                if seeded && !self.backend.volume_exists(&cache).await {
                    if let Err(error) = self
                        .backend
                        .clone_volume(&environment.cache_volume, &cache)
                        .await
                    {
                        tracing::warn!(session = %id, %error, "session cache starts empty");
                    }
                }
                Some(cache)
            }
            None => None,
        };
        // What an install writes beside the source (`node_modules`,
        // `vendor/`, `.venv`) is in no cache and not in the workspace Git
        // seeded, so the repository's own preparation runs on the workspace
        // before the agent arrives, unless the snapshot the session starts
        // from was prepared at its very commit. Its outcome goes in the
        // orientation.
        let prepared_at = match (&origin, image.from_snapshot(&self.cfg), &image.image) {
            (Some(origin), true, Some(snapshot)) => {
                crate::sandbox_images::prepared_from(&self.store, origin, snapshot)
            }
            _ => None,
        };
        let from = match (image.from_snapshot(&self.cfg), &image.image) {
            (true, Some(snapshot)) => Some(FromSnapshot {
                snapshot,
                prepared_at: prepared_at.as_deref(),
                base: base.as_deref(),
            }),
            _ => None,
        };
        let prepared = self
            .prepare_workspace(
                id,
                &slug,
                &workspace,
                session_cache.as_deref().ok_or_else(|| {
                    // Said precisely: on a runtime that cannot build images (a
                    // Kubernetes node without a builder) this is every
                    // session, and the reason is the runtime's, not the
                    // repository's.
                    let why = image
                        .note
                        .as_deref()
                        .map(|note| format!(": {note}"))
                        .unwrap_or_default();
                    format!(
                        "the session runs in the harness's own image, not its repository's{why}"
                    )
                }),
                &environment,
                from,
                started,
            )
            .await;
        if !self.startable(id)? {
            self.tokens.lock().await.remove(id);
            return Ok(());
        }

        let mut wiring = wiring;
        // The workspace's read-only `origin`, through the gateway, for a
        // session on a forge repository this node cloned, when the workspace
        // names it as this launch would. The gateway resolves the repository
        // from the session again on every request; this is only the URL the
        // workspace knows it by, and the harness's token is sent to it.
        wiring.git_origin = origin
            .as_deref()
            .and_then(|repo| {
                crate::gateway::git::origin_for(
                    &crate::config::Config::state_dir(),
                    repo,
                    &self.backend.harness_host(),
                    self.cfg.gateway.forward_port,
                    &spec.channel,
                )
            })
            .filter(|url| workspace_origin.as_deref() == Some(url.as_str()));
        let manifest = {
            let (skills, instructions, agents) = self.store.manifest_contents(&spec.channel)?;
            let built = crate::manifest::build(crate::manifest::Inputs {
                channel: &spec.channel,
                skills: crate::manifest::with_builtins(skills),
                instructions,
                agents,
                providers: wiring.providers.iter().map(|p| p.name.clone()).collect(),
                policy_revision: self.policy.read().version.to_string(),
            })
            .map_err(|e| anyhow::anyhow!("the launch manifest for `{}`: {e}", spec.channel))?;
            self.store.manifest_record(&built)?
        };
        self.store.update_session(
            id,
            SessionPatch {
                manifest_digest: Some(manifest.digest.clone()),
                ..Default::default()
            },
        )?;
        wiring.manifest = manifest.clone();

        // What the session is told first: this node's facts, the work and its
        // constraints, the channel's policy and what is known, and then
        // whatever conventions fit under the cap. Recorded as an event so the
        // transcript shows what the agent was told — and what it was not.
        // On DigitalOcean the harness's home is the stock base's, whatever
        // this node's own runtime uses: its state is placed where it looks.
        let harness_home = match placed.is_elsewhere() {
            true => crate::sandbox_images::HOME.to_string(),
            false => self.backend.harness_home(),
        };
        let scratchpad = saved::scratchpad_target(&harness_home);
        let (orientation, missing) = {
            let session_row = self.store.get_session(id)?;
            let project = session_row
                .as_ref()
                .and_then(|s| s.project_id.clone())
                .and_then(|pid| self.store.project_get(&pid).ok().flatten());
            let node = self.store.get_node(&self.node_id)?;
            let item = spec
                .work_item_id
                .as_deref()
                .and_then(|i| self.store.work_get(i).ok().flatten());
            let plan_body = item
                .as_ref()
                .and_then(|i| i.phase_plan_slug.as_deref())
                .and_then(|slug| self.store.doc_get(&spec.channel, slug).ok().flatten())
                .map(|d| d.body);
            let ready = self
                .store
                .work_ready(&spec.channel, project.as_ref().map(|p| p.id.as_str()))
                .unwrap_or_default();
            let review = spec
                .review_id
                .as_deref()
                .and_then(|r| self.store.get_review(r).ok().flatten());
            // A reviewer told it can run the tests when it cannot spends its
            // budget finding that out. Why it has no image says what to fix:
            // a review tied to no repository, or a repository with no image.
            let no_toolchain = image.image.is_none().then(|| match &origin {
                None => "no repository was resolved for this review, so no `[[repo]]` image \
                         could be chosen"
                    .to_string(),
                Some(_) => image.note.clone().unwrap_or_else(|| {
                    "no `[[repo]]` entry for its repository names an image".to_string()
                }),
            });
            // What its workspace holds, not what a resubmission recorded
            // while it started.
            let reviewed = session_row
                .as_ref()
                .filter(|_| review.is_some())
                .and_then(|row| self.store.review_session_revision(row).ok().flatten());
            // Resolved from `origin` when the row was created, and read back
            // the way the forge tools read it when they default to it, so
            // what the session is told and what a call without `repo` reaches
            // cannot differ.
            let forge_repo = self
                .store
                .session_remote(id)?
                .as_deref()
                .and_then(crate::forge::ForgeRepo::from_remote);
            let tool_names: Vec<String> = self
                .tools
                .list_for(&spec.channel, &self.node_id, spec.phase)
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect();
            let namespaces = self.tools.tool_namespaces(&spec.channel, &self.node_id);
            let services: Vec<String> = self
                .tools
                .cfg
                .service
                .iter()
                .map(|service| service.name.clone())
                .collect();
            let policy = self.policy.read();
            crate::corpus::orientation::assemble(
                &self.store,
                &policy,
                &crate::corpus::orientation::Facts {
                    node_name: node.as_ref().map(|n| n.name.as_str()).unwrap_or(""),
                    node_id: &self.node_id,
                    backend: self.backend.kind(),
                    harness: adapter.id(),
                    harness_version: adapter.pinned_version(),
                    channel: &spec.channel,
                    project_id: project.as_ref().map(|p| p.id.as_str()),
                    project_name: project.as_ref().map(|p| p.name.as_str()),
                    tools: &tool_names,
                    namespaces: &namespaces,
                    services: &services,
                    worktree: "/work",
                    phase: spec.phase.as_str(),
                    item: item.as_ref(),
                    plan_body: plan_body.as_deref(),
                    ready: &ready,
                    review: review
                        .as_ref()
                        .map(|row| crate::corpus::orientation::ReviewFacts {
                            row,
                            session_id: id,
                            no_toolchain: no_toolchain.as_deref(),
                            revision: reviewed.as_ref(),
                            checks_configured: !environment.checks.is_empty(),
                        }),
                    manifest: &manifest,
                    egress: egress
                        .as_ref()
                        .map(|_| environment.session_egress.as_slice()),
                    forge_repo: forge_repo.as_ref(),
                    git_origin: wiring.git_origin.is_some(),
                    prepared: prepared.as_ref(),
                    stale_base: stale.as_deref(),
                    scratchpad: Some(&scratchpad),
                },
            )
        };
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::ORIENTATION.into(),
            ref_id: None,
            // `missing` names each piece the cap kept out, so the operator
            // reading the log sees what the session was not told rather than
            // a bare flag that something, somewhere, was cut.
            payload: json!({
                "text": orientation,
                "trimmed": !missing.is_empty(),
                "missing": missing,
                "chars": orientation.len(),
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        if !self.startable(id)? {
            self.tokens.lock().await.remove(id);
            return Ok(());
        }

        // The harness reaches the node only through the gateway's forward, and
        // only with this session's token. Tools are offered only if the
        // channel has a credential bound to it; otherwise the harness is given
        // no MCP server at all rather than one that refuses everything.
        let mcp_servers = if self
            .tools
            .list_for(&spec.channel, &self.node_id, spec.phase)
            .is_empty()
        {
            Vec::new()
        } else {
            vec![json!({
                "type": "http",
                "name": "tracon",
                "url": format!(
                    "http://{}:{}/mcp/{id}",
                    self.backend.harness_host(),
                    self.cfg.gateway.forward_port
                ),
                "headers": [{ "name": "Authorization", "value": format!("Bearer {token}") }],
            })]
        };
        // Staged into the harness's own configuration too: Claude Code reads
        // its MCP servers from a config file.
        wiring.mcp_servers = mcp_servers.clone();

        // Whose name the session's commits carry: the account behind the
        // channel's bound forge credential, else the host's own Git identity.
        let identity = self.commit_identity(&spec.channel, &repo).await;
        let scratch = materialize::scratch_for(
            id,
            &snapshot,
            &repo,
            &harness_home,
            adapter.as_ref(),
            &wiring,
            &orientation,
            identity.as_ref(),
        )?;
        // A session from before its state was its own (an upgraded node's)
        // takes what its resume needs out of the runtime's volumes.
        if let Some(reopened) = reopened
            .as_ref()
            .filter(|_| self.cfg.runtime.kind == crate::config::RuntimeKind::Podman)
        {
            materialize::carry_from_runtime_volumes(
                self.backend.as_ref(),
                &scratch,
                &reopened.resume.harness_session_id,
            )
            .await;
        }
        self.backend
            .import_volume(&scratch.volume, &scratch.dir)
            .await
            .map_err(anyhow::Error::msg)?;
        let container = crate::config::resource_name(&format!("h-{slug}"));
        let mut mounts = scratch.mounts;
        materialize::own_state(&mut mounts, &scratch.volume);
        mounts.push(workspace.mount(SANDBOX_WORKDIR, false));
        // A session whose pod this node's runtime cannot schedule is placed
        // again with that runtime counted full, once: on to DigitalOcean
        // where it may go (or the operator asked), else ended with the
        // scheduler's words.
        let mut placed = placed;
        let mut template_retried = false;
        let (runner, started_elsewhere, elsewhere_note, relaunch, handle, events) = loop {
            // The image a session placed elsewhere started from: its
            // template's there, which `image` (this node's runtime's) knows
            // nothing of.
            let mut started_elsewhere = None;
            // Why that is not what its repository gets on this node's own
            // runtime, when it is not.
            let mut elsewhere_note = None;
            let (runner, cache_env) = match &placed {
                placing::Placed::DigitalOcean {
                    runtime,
                    requirements,
                } => {
                    // Its repository's prepared snapshot where that has a
                    // template there (this node's, or a peer's by portable key).
                    let image = runtime
                        .session_image(
                            self.backend.as_ref(),
                            &self.cfg,
                            &self.store,
                            origin.as_deref(),
                            &environment,
                        )
                        .await
                        .ok_or_else(|| {
                            anyhow::anyhow!("DigitalOcean has no image to run this session in yet")
                        })?;
                    if let Some(note) = &image.note {
                        tracing::info!(session = %id, %note, "a session on DigitalOcean runs without its repository's own image");
                    }
                    elsewhere_note = image.note;
                    let image = image.image;
                    started_elsewhere = Some(image.clone());
                    // Its trees are kept where this node's own sandboxes keep
                    // theirs, which is where its workspace was made.
                    let files = self
                        .backend
                        .sandbox_files_store()
                        .unwrap_or_else(crate::runner::sandbox_files::FilesStore::for_node);
                    (
                        runtime.runner_with(
                            &self.cfg,
                            requirements.clone(),
                            mounts.clone(),
                            image,
                            adapter.id(),
                            files,
                        ),
                        Vec::new(),
                    )
                }
                placing::Placed::Local => {
                    // Kept with the sandbox if the node stops while it runs.
                    crate::runner::reattach::note_image(&container, &image);
                    session_runner(
                        self.backend.as_ref(),
                        adapter.id(),
                        image.image.as_deref(),
                        session_cache.clone(),
                        mounts.clone(),
                    )
                }
            };

            // Record the container name before it exists: it is deterministic, and a
            // launch that fails after the container is created would otherwise leave
            // a credential-mounted harness with no name in the store for
            // `reconcile_after_restart` to remove.
            self.store.update_session(
                id,
                SessionPatch {
                    container_name: Some(container.clone()),
                    ..Default::default()
                },
            )?;

            let (mut harness_env, harness_secret_env) =
                harness_environment(&wiring, egress.as_ref(), &harness_home);
            harness_env.extend(cache_env);

            let launch_spec = LaunchSpec {
                cwd_in_runner: SANDBOX_WORKDIR.into(),
                model: spec.model.clone(),
                container_name: container.clone(),
                harness_home: harness_home.clone(),
                mcp_servers: mcp_servers.clone(),
                tools: self.cfg.harness.tools.clone(),
                env: harness_env,
                secret_env: harness_secret_env,
                system_prompt_file: Some(scratch.orientation_path.clone()),
                resume: reopened.as_ref().map(|r| r.resume.clone()),
                labels: crate::runner::RunLabels {
                    channel: Some(spec.channel.clone()),
                    phase: Some(spec.phase.as_str()),
                    ..crate::runner::RunLabels::of(crate::runner::RunKind::Session)
                }
                // From the repository the work came from: a continued session's
                // own path is `workspace://<session>`, which names no repository.
                .repo_dir(origin.as_deref()),
            };
            // Asked before anything runs in it, so a conversation the harness
            // has no record of refuses the reopening rather than its first turn.
            if let Some(reopened) = &reopened {
                let kept = adapter
                    .has_conversation(
                        runner.as_ref(),
                        &launch_spec,
                        &reopened.resume.harness_session_id,
                    )
                    .await;
                if let Ok(false) = kept {
                    return Err(crate::adapter::AdapterError::ConversationGone(format!(
                        "the {} harness no longer has a record of conversation {}",
                        adapter.id(),
                        reopened.resume.harness_session_id
                    ))
                    .into());
                }
            }
            // Kept for a harness that can be started again on this session when
            // its process dies, in a sandbox on this node's own runtime. One
            // placed elsewhere is gone by the time its exit is known, and a
            // relaunch would be a new paid sandbox of the same size that
            // would likely die the same way: the session ends instead, to be
            // carried on (`continue_interrupted`).
            let relaunch =
                (adapter.restorable() && !placed.is_elsewhere()).then(|| supervisor::Relaunch {
                    adapter: adapter.clone(),
                    spec: launch_spec.clone(),
                });
            // A sandbox on this node's runtime may wait for a node to be
            // added for it first.
            let within = match placed.is_elsewhere() {
                true => STARTUP_TIMEOUT,
                false => STARTUP_TIMEOUT + self.backend.start_allowance(),
            };
            let launched =
                tokio::time::timeout(within, adapter.launch(runner.as_ref(), launch_spec)).await;
            match launched {
                Ok(Ok((handle, events))) => {
                    break (
                        runner,
                        started_elsewhere,
                        elsewhere_note,
                        relaunch,
                        handle,
                        events,
                    )
                }
                Ok(Err(e)) => {
                    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                    if placed.is_elsewhere() && !template_retried && template_gone(&e) {
                        tracing::warn!(session = %id, error = %e, "the session's DigitalOcean template is gone; choosing its image again");
                        template_retried = true;
                        continue;
                    }
                    let Some(said) = unschedulable(&e).filter(|_| !placed.is_elsewhere()) else {
                        return Err(e.into());
                    };
                    tracing::warn!(session = %id, %said, "the session's pod could not be scheduled; placing it again");
                    crate::runner::reattach::unnote_image(&container);
                    placed = match self
                        .place(id, &spec.channel, origin.as_deref(), started, Some(&said))
                        .await?
                    {
                        // Full here: placement never keeps it on this
                        // runtime, but if it did, the pod still has no room.
                        placing::Placed::Local => return Err(e.into()),
                        elsewhere => elsewhere,
                    };
                }
                Err(_) => {
                    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                    return Err(anyhow::anyhow!(
                        "harness startup timed out after {} seconds",
                        within.as_secs()
                    ));
                }
            }
        };
        let (cmd_tx, cmd_rx) = mpsc::channel(16);

        if !self.startable(id)? {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
            self.tokens.lock().await.remove(id);
            return Ok(());
        }
        // What the harness turned out to be, next to what this node expected of
        // it. The pin and the protocol were both checked before the handshake
        // returned, so this records a session that is already known compatible
        // — for reading the transcript later against the build that produced it.
        let compat = handle.compat();
        self.store.update_session(
            id,
            SessionPatch {
                harness_session_id: Some(handle.harness_session_id().to_string()),
                harness_agent: Some(compat.agent.clone()),
                harness_found: Some(compat.version.clone()),
                harness_protocol: Some(compat.protocol.clone()),
                ..Default::default()
            },
        )?;
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::SESSION_STARTED.into(),
            ref_id: None,
            payload: json!({
                "model": spec.model, "model_source": model_source,
                "harness": adapter.id(), "harness_source": harness_source,
                "phase": spec.phase.as_str(),
                "work_item_id": spec.work_item_id,
                "policy_version": self.policy.read().version,
                "harness_agent": compat.agent,
                "harness_version": compat.version,
                "harness_expected": adapter.pinned_version(),
                "harness_protocol": compat.protocol,
                "image": started_elsewhere.clone().unwrap_or_else(|| image.reported(&self.cfg)),
                "image_source": match &started_elsewhere {
                    Some(started_in) => crate::repo_image::source_elsewhere(
                        started_in,
                        &crate::sandbox_images::base_image(&self.cfg),
                        &environment.image,
                    ),
                    None => image.source(&self.cfg),
                },
                "toolchain_image": image.toolchain_image,
                "image_note": elsewhere_note.as_ref().or(image.note.as_ref()),
                "resumed": reopened.is_some(),
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });
        let series = session_series(&self.store, id, &spec, adapter.id());
        crate::telemetry::telemetry().session_started(
            &series,
            match image.image {
                Some(_) => "repository",
                None => "harness",
            },
            started.elapsed(),
        );
        if let Some(reopened) = &reopened {
            self.record(NewEvent {
                session_id: id.to_string(),
                work_item_id: None,
                kind: ek::CONVERSATION_RESUMED.into(),
                ref_id: None,
                payload: json!({
                    "outcome": "resumed",
                    "harness_session_id": reopened.resume.harness_session_id,
                    "from_state": reopened.from.state,
                    "end_reason": reopened.from.end_reason,
                    "compact": reopened.compact,
                }),
                at_ms: now_ms(),
                mono_ms: started.elapsed().as_millis() as i64,
            });
        }

        let cmd_tx_for_turns = cmd_tx.clone();
        if let Some(text) = spec.initial_prompt.filter(|text| !text.trim().is_empty()) {
            let (ack, _wait) = oneshot::channel();
            cmd_tx_for_turns
                .send(Command::Prompt { text, ack })
                .await
                .map_err(|_| anyhow::anyhow!("session supervisor did not start"))?;
        }
        if reopened.as_ref().is_some_and(|r| r.compact) {
            let (ack, _wait) = oneshot::channel();
            cmd_tx_for_turns
                .send(Command::Compact { ack })
                .await
                .map_err(|_| anyhow::anyhow!("session supervisor did not start"))?;
        }
        self.live.lock().await.insert(id.to_string(), cmd_tx);

        let sup = Supervisor::new(
            id.to_string(),
            self.node_id.clone(),
            self.store.clone(),
            self.bus.clone(),
            Arc::from(handle),
            started,
            Duration::from_secs(self.cfg.live().session.permission_timeout_secs),
            cmd_tx_for_turns,
            runner,
            container.clone(),
            self.policy.clone(),
            spec.channel.clone(),
        );
        let sup = match relaunch {
            Some(relaunch) => sup.with_relaunch(relaunch),
            None => sup,
        }
        .with_checks(environment.checks)
        .with_waits(self.waits())
        .with_kickoff(kickoff)
        .with_series(series)
        .with_saver(saved::Saver::new(self.backend.clone(), adapter.clone(), id));
        let live = self.live.clone();
        let tokens = self.tokens.clone();
        let waits = self.waits();
        let sid = id.to_string();
        let noted = container.clone();
        let manager = self.clone();
        tokio::spawn(async move {
            sup.run(events, cmd_rx).await;
            // Any record of its sandbox was written as the node stopped.
            crate::runner::reattach::unnote_image(&noted);
            live.lock().await.remove(&sid);
            // A close sent while it was suspending was never read.
            manager.end_if_item_closed(&sid);
            waits.forget(&sid);
            // The token dies with the session; a later call with it is refused.
            tokens.lock().await.remove(&sid);
            // And its way out with it.
            drop(egress);
            materialize::remove(&sid);
        });
        Ok(())
    }

    /// A channel a session may run on: one this node holds, or one of the two
    /// labels a standalone node offers before any channel is created.
    pub fn channel_usable(&self, channel: &str) -> Result<(), SessionError> {
        let known = match self.store.channel_list() {
            Ok(rows) if rows.iter().any(|c| !c.name.starts_with('@')) => rows
                .iter()
                .any(|c| c.name == channel && !c.name.starts_with('@')),
            // No channels yet: the interface offers these two, so they work.
            Ok(_) => crate::http::api::DEFAULT_CHANNELS.contains(&channel),
            Err(e) => return Err(e.into()),
        };
        if !known {
            return Err(SessionError::UnknownChannel(channel.to_string()));
        }
        if !self.bindings(channel)["archived"].is_null() {
            return Err(SessionError::ChannelArchived(channel.to_string()));
        }
        Ok(())
    }

    async fn send(&self, id: &str, cmd: Command) -> Result<(), SessionError> {
        let tx = self.live.lock().await.get(id).cloned();
        match tx {
            Some(tx) => tokio::time::timeout(COMMAND_SEND_TIMEOUT, tx.send(cmd))
                .await
                .map_err(|_| {
                    SessionError::Rejected(
                        "session supervisor did not accept the command in time".into(),
                    )
                })?
                .map_err(|_| SessionError::Rejected("session is no longer running".into())),
            // Not live here. Someone else's, or one of ours that has ended.
            None => match self.store.get_session(id)? {
                Some(row) if row.node_id != self.node_id => {
                    Err(SessionError::Remote(row.node_id, row.channel))
                }
                _ => Err(SessionError::Rejected(
                    "session is not running on this node".into(),
                )),
            },
        }
    }

    /// Prompt a session, or hold the prompt for it while its turn runs.
    pub async fn prompt(&self, id: &str, text: String) -> Result<Prompted, SessionError> {
        self.ensure_active(id)?;
        self.refuse_turn_while_restarting(id)?;
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                id,
                Command::Prompt {
                    text: text.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Prompt {
                        session_id: id.to_string(),
                        text,
                    },
                    true,
                )
                .await
                .map(|answer| match answer["queued_message"].as_str() {
                    Some(message_id) => Prompted::Queued {
                        message_id: message_id.to_string(),
                    },
                    None if answer["queued_for_owner"] == true => Prompted::QueuedForOwner {
                        node_id: node.clone(),
                    },
                    None => Prompted::Sent,
                }),
            Err(e) => Err(e),
        }
    }

    /// [`Self::prompt`], with files the operator attached: each is checked
    /// against [`attachments`]' limits and refused with why, then staged into
    /// the session's workspace before the prompt is sent, and the prompt's
    /// text names each one. Answers what was staged.
    pub async fn prompt_with(
        &self,
        id: &str,
        text: String,
        uploads: &[attachments::Upload],
    ) -> Result<(Prompted, Vec<attachments::Staged>), SessionError> {
        if uploads.is_empty() {
            return Ok((self.prompt(id, text).await?, Vec::new()));
        }
        self.ensure_active(id)?;
        self.refuse_turn_while_restarting(id)?;
        let staged = self.stage_attachments(id, uploads).await?;
        let prompted = self
            .prompt(id, attachments::with_note(&text, &staged))
            .await?;
        Ok((prompted, staged))
    }

    /// [`Self::ask`] with a message and files the operator attached, staged
    /// as [`Self::prompt_with`] stages them, once the session is known to be
    /// askable.
    pub async fn ask_with(
        &self,
        id: &str,
        text: String,
        uploads: &[attachments::Upload],
    ) -> Result<SessionRow, SessionError> {
        if uploads.is_empty() {
            return self.ask(id, Ask::Message(text)).await;
        }
        let askable = self.askable(id).await?;
        if let Some(reason) = askable.reason.filter(|_| !askable.restorable) {
            return Err(SessionError::Rejected(format!(
                "its conversation cannot be restored: {reason}"
            )));
        }
        let staged = self.stage_attachments(id, uploads).await?;
        self.ask(id, Ask::Message(attachments::with_note(&text, &staged)))
            .await
    }

    /// Check `uploads` and write them into the session `id`'s workspace under
    /// [`attachments::DIR`], recording what was staged on the session.
    async fn stage_attachments(
        &self,
        id: &str,
        uploads: &[attachments::Upload],
    ) -> Result<Vec<attachments::Staged>, SessionError> {
        let files = attachments::accept(uploads).map_err(SessionError::Attachment)?;
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        // The workspace is where the session runs: a peer's is the peer's,
        // and the mesh carries a prompt's text, never its files.
        if row.node_id != self.node_id {
            return Err(SessionError::Attachment(format!(
                "this session runs on node {}, and attachments are staged by the node that                  runs it: send them from that node's own interface",
                row.node_id
            )));
        }
        if row.harness_id == external::HARNESS_ID {
            return Err(SessionError::Attachment(
                "an external agent works in its own checkout; the node has no workspace of                  its to stage files into"
                    .into(),
            ));
        }
        let volume = crate::workspace::volume_name(&workspace_of(&row));
        let place = attachments::place(self.backend().as_ref(), &volume)
            .map_err(SessionError::Attachment)?;
        let prompt_id = uuid::Uuid::now_v7().simple().to_string();
        let stage = crate::workspace::staging_path(&format!("attach-{prompt_id}"));
        let _ = std::fs::remove_dir_all(&stage);
        let written = match attachments::lay_out(&stage, &prompt_id, &files) {
            Ok(()) => attachments::write(place, &stage).await,
            Err(error) => Err(format!("the attachments could not be staged: {error}")),
        };
        let _ = std::fs::remove_dir_all(&stage);
        written.map_err(SessionError::Rejected)?;
        let staged = attachments::staged(&prompt_id, &files);
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::ATTACHMENTS_STAGED.into(),
            ref_id: Some(prompt_id.clone()),
            payload: json!({
                "dir": attachments::prompt_dir(&prompt_id),
                "attachments": staged,
            }),
            at_ms: now_ms(),
            mono_ms: 0,
        });
        Ok(staged)
    }

    /// Deliver what the operator queued for `session_id` in the result of
    /// the node tool `tool` it just called: the text to append, with what it
    /// is and where it came from, or `None` when nothing was waiting. Each
    /// message is claimed once, so it is never also sent as the next prompt.
    pub fn deliver_queued_in_tool_result(&self, session_id: &str, tool: &str) -> Option<String> {
        let held = self
            .store
            .claim_queued_prompts(session_id, &format!("tool:{tool}"))
            .ok()
            .filter(|held| !held.is_empty())?;
        self.record_event(
            session_id,
            ek::PROMPT_DELIVERED,
            json!({
                "message_ids": held.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
                "tool": tool,
            }),
        );
        let said = held
            .iter()
            .map(|q| q.text.trim())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        let plural = if held.len() == 1 {
            "a message"
        } else {
            "messages"
        };
        Some(format!(
            "[tracon] The operator sent you {plural} while this turn was running. \
             It is from the operator, not part of the {tool} result above; read it \
             before you carry on.\n\n{said}"
        ))
    }

    pub async fn answer(&self, id: &str, answer: OperatorAnswer) -> Result<(), SessionError> {
        // An approval this node holds is answered here, not by a session:
        // nothing is waiting on it, and the node runs the call itself.
        if let Some(approval) = self.store.get_approval(id)? {
            if approval.node_id == self.node_id {
                return self.tools.answer_approval(id, answer).await;
            }
            return self
                .forward(&approval.node_id, answer.into_frame(id), false)
                .await
                .map(|_| ());
        }
        let perm = self
            .store
            .get_permission(id)?
            .ok_or(SessionError::NotFound)?;
        // A peer's card this node closed (its owner left the channel, or
        // stopped listing it) is not relayed: the owner would refuse it.
        if perm.node_id != self.node_id && perm.state != "new" {
            return Err(SessionError::Rejected(format!(
                "this approval is no longer waiting ({})",
                perm.state
            )));
        }
        let Some(session_id) = perm.session_id else {
            // A peer's approval card with no session behind it: the peer
            // holds the approval, decides it and runs it.
            if perm.node_id != self.node_id {
                return self
                    .forward(&perm.node_id, answer.into_frame(id), false)
                    .await
                    .map(|_| ());
            }
            return Err(SessionError::NotFound);
        };
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                &session_id,
                Command::Answer {
                    permission_id: id.to_string(),
                    option_id: answer.option_id.clone(),
                    arguments: answer.arguments.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(&node, answer.into_frame(id), false)
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// An approval's history goes to the log of whoever asked: the session,
    /// or the channel's external log.
    fn record_approval(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        kind: &str,
        payload: serde_json::Value,
    ) {
        match &approval.session_id {
            Some(session_id) => self.record(NewEvent {
                session_id: session_id.clone(),
                work_item_id: None,
                kind: kind.into(),
                ref_id: Some(approval.id.clone()),
                payload,
                at_ms: now_ms(),
                mono_ms: 0,
            }),
            None => self.record_external(
                &approval.channel,
                approval.lane.as_deref(),
                kind,
                Some(&approval.id),
                payload,
            ),
        }
    }

    /// A call was held for the operator: put it in the waiting bay and say so
    /// in the asking session's log. The session itself goes on working.
    pub async fn approval_requested(&self, approval: &crate::store::approvals::ApprovalRow) {
        self.record_approval(
            approval,
            ek::PERMISSION_REQUEST,
            json!({
                "approval_id": approval.id, "permission_id": approval.id,
                "title": approval.title, "kind": "tool", "tool": approval.tool,
                "raw_input": { "tool": approval.tool, "arguments": serde_json::from_str::<serde_json::Value>(&approval.arguments).unwrap_or_default() },
                "expires_ms": approval.expires_ms,
            }),
        );
        self.publish_queue().await;
    }

    /// An approval moved on: answered, run, or expired.
    pub async fn approval_decided(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        state: &str,
    ) {
        self.approval_answered(approval, state, None).await
    }

    /// [`Self::approval_decided`], naming who answered it. The answer's record
    /// carries what the audit shows: who, and whether what runs is the
    /// operator's edit rather than the call as asked.
    pub async fn approval_answered(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        state: &str,
        by: Option<&str>,
    ) {
        use crate::store::approvals as a;
        let kind = match state {
            a::RUNNING | a::REJECTED | a::CHANGES_REQUESTED => ek::PERMISSION_ANSWER,
            a::EXPIRED => ek::PERMISSION_EXPIRED,
            _ => ek::APPROVAL_SETTLED,
        };
        let mut payload = json!({
            "approval_id": approval.id, "permission_id": approval.id,
            "tool": approval.tool, "state": state, "reason": approval.reason,
            "option_id": approval.answer_option_id, "notes": approval.operator_note,
        });
        if kind == ek::PERMISSION_ANSWER {
            payload["decided_by"] = json!(by.unwrap_or("operator"));
            payload["arguments_hash"] = json!(approval.arguments_hash());
            if let Some(edited) = approval
                .edited_arguments
                .as_deref()
                .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
            {
                payload["edited"] = json!(true);
                payload["edited_arguments"] = crate::mcp::audit::redacted(&edited);
            }
        }
        self.record_approval(approval, kind, payload);
        self.publish_queue().await;
        if let Some(session_id) = &approval.session_id {
            self.wait_moved(session_id).await;
        }
    }

    /// Whether an approved call may still run for this session. A session
    /// that has ended does not stop it: the approval belongs to the channel.
    /// A paused session, or a channel the operator stopped, does.
    pub fn approval_runnable(&self, ctx: &crate::mcp::CallContext) -> Result<(), SessionError> {
        self.channel_usable(&ctx.channel)?;
        match ctx.session_id() {
            None => self.external_open(&ctx.channel),
            Some(id) => {
                if let Some(row) = self.store.get_session(id)? {
                    if row.state == SessionState::Paused.as_str() {
                        return Err(SessionError::Rejected("session is paused".into()));
                    }
                }
                Ok(())
            }
        }
    }

    /// Whether the caller may make a brokered call now: its session is live,
    /// or, for a harness the operator runs themselves, its channel is open to
    /// external harnesses.
    pub fn caller_active(&self, ctx: &crate::mcp::CallContext) -> Result<(), SessionError> {
        match ctx.session_id() {
            Some(id) => self.ensure_active(id),
            None => {
                self.channel_usable(&ctx.channel)?;
                self.external_open(&ctx.channel)
            }
        }
    }

    /// External harnesses on `channel` are not stopped by the operator.
    pub fn external_open(&self, channel: &str) -> Result<(), SessionError> {
        let bindings = self.bindings(channel);
        // A channel-wide pause from an older node fences exactly like a Stop.
        if bindings["external_stopped"] == true || bindings["external_paused"] == true {
            return Err(SessionError::Rejected(
                "external broker access was stopped by the operator; start it again in Settings or with `tracon external start <channel>`".into(),
            ));
        }
        Ok(())
    }

    /// Expire approvals nobody answered in time.
    pub async fn expire_approvals(&self) {
        let Ok(expired) = self.store.expire_due_approvals() else {
            return;
        };
        for approval in &expired {
            self.approval_decided(approval, crate::store::approvals::EXPIRED)
                .await;
        }
    }

    /// The item a session holds was closed: record it and end the session
    /// once its turn is over. A suspended one here, which has no turn and no
    /// supervisor, ends now. A session not live here (ended, or another
    /// node's) is left alone; the close itself replicates, and the node that
    /// owns a suspended one ends it there (`end_closed_suspended`).
    pub async fn item_closed(&self, id: &str, summary: &str) {
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::WORK_CLOSED.into(),
            ref_id: None,
            payload: json!({ "summary": summary }),
            at_ms: now_ms(),
            mono_ms: 0,
        });
        if self
            .send(id, Command::EndAfterTurn(EndReason::ItemClose))
            .await
            .is_ok()
        {
            return;
        }
        if let Ok(Some(row)) = self.store.get_session(id) {
            if row.node_id == self.node_id && row.state == SessionState::Suspended.as_str() {
                let _ = self.end_suspended(id, EndReason::ItemClose, json!({}));
            }
        }
    }

    /// Mark a session as waiting on deterministic checks (or back to running)
    /// and tell the interface. Set by the submit tool inside a turn, and
    /// cleared by the node-owned task running the checks, which can finish
    /// after that turn ended. A check can outlive the session that asked for
    /// it, so both ends of this are fenced in the UPDATE itself rather than
    /// by a read first: a pause or a stop landing mid-check wins, and the
    /// check finishing afterwards must not put the row back into `running`.
    pub fn set_checking(&self, id: &str, checking: bool) {
        let state = if checking {
            SessionState::WaitingOnCheck
        } else {
            SessionState::Running
        };
        self.set_check_state(id, state);
    }

    /// Mark a session's checks as waiting in the node's line for room,
    /// fenced as [`Self::set_checking`] is.
    pub fn set_check_queued(&self, id: &str) {
        self.set_check_state(id, SessionState::QueuedForCheck);
    }

    fn set_check_state(&self, id: &str, state: SessionState) {
        let mut fenced: Vec<&str> = SessionState::TERMINAL.to_vec();
        fenced.push(SessionState::Paused.as_str());
        match self
            .store
            .update_session_unless(id, &fenced, SessionPatch::state(state.as_str()))
        {
            Ok(true) => {}
            _ => {
                self.refused_transition(id, "set_checking", json!({ "attempted": state.as_str() }));
                return;
            }
        }
        if let Ok(Some(row)) = self.store.get_session(id) {
            self.bus.publish(Frame::Session(Box::new(row)));
        }
    }

    /// Record a transition refused because the session had already ended or
    /// been fenced. The same shape the supervisor writes, for the writers
    /// that live outside it.
    pub fn refused_transition(&self, id: &str, what: &str, mut detail: serde_json::Value) {
        let state = self
            .store
            .get_session(id)
            .ok()
            .flatten()
            .map(|row| row.state)
            .unwrap_or_else(|| "gone".into());
        detail["what"] = json!(what);
        detail["state"] = json!(state);
        tracing::warn!(session = %id, %what, %state, "refused a late transition");
        self.record_event(id, ek::LATE_REFUSED, detail);
    }

    /// Watch the backend's grants: each host a session's own grant refuses
    /// is put to the operator, and recorded on the session once.
    /// Hear how every sandbox's take-backs go: what is worth it becomes a
    /// `workspace_sync` event on the sandbox's session, and a run of
    /// failures tells the operator. Installed once the node is up; the last
    /// manager to install it hears them.
    pub fn report_workspace_syncs(&self) {
        let manager = self.clone();
        crate::runner::sandbox_files::set_reporter(Some(Arc::new(move |report| {
            manager.workspace_synced(&report)
        })));
    }

    /// One take-back reported: recorded on its session when worth it, and
    /// pushed to the operator at the failure that makes a run of them. A
    /// sandbox that is no session's of this node (a preparation's) is only
    /// in the log.
    pub fn workspace_synced(&self, report: &crate::runner::sandbox_files::SyncReport) {
        let session = match self
            .store
            .session_by_container(&self.node_id, &report.sandbox)
        {
            Ok(Some(session)) => session,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(sandbox = %report.sandbox, %error, "could not find the session a take-back was for");
                return;
            }
        };
        if report.record {
            self.record_event(
                &session.id,
                ek::WORKSPACE_SYNC,
                workspace_sync_payload(report),
            );
        }
        let Some(notification) =
            crate::notify::Notification::workspace_sync(&session.id, &session.branch, report)
        else {
            return;
        };
        if !crate::notify::channel_pushes(&self.store, &session.channel) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (store, cfg) = (self.store.clone(), self.cfg.clone());
        runtime.spawn(async move {
            let now = crate::store::now_ms();
            for device in store.push_subscriptions_live(now).unwrap_or_default() {
                crate::notify::deliver(&store, &cfg, &device, &notification, now).await;
            }
        });
    }

    pub fn record_egress_refusals(&self) {
        if let Some(grants) = self.backend.egress_grants() {
            self.watch_egress(grants);
        }
    }

    /// Turn the refusals under `grants` into asks, and widen these grants
    /// when the operator allows one.
    pub fn watch_egress(&self, grants: &crate::gateway::proxy::Grants) {
        *self.egress.write() = Some(grants.clone());
        grants.on_refusal(self.egress_refusal_observer());
    }

    /// The grants an egress answer widens, once they are watched.
    pub fn egress_grants(&self) -> Option<crate::gateway::proxy::Grants> {
        self.egress.read().clone()
    }

    /// What `watch_egress` installs. A session's refusal raises the
    /// operator's ask, or finds the one already waiting, and the refusal's
    /// reason says so; the event is recorded once per session and host, as a
    /// package manager retries and the operator needs to hear it once. A
    /// repository's warm-up asks to save the host to its egress instead; any
    /// other grant that is no session's (a candidate's preparation) is
    /// refused as it was, and its failure names the host.
    pub fn egress_refusal_observer(&self) -> crate::gateway::proxy::OnRefusal {
        let manager = self.clone();
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        Arc::new(move |grant, host| {
            if let Some(repo) = &grant.warming {
                let host = crate::mcp::egress::normalize_host(host).ok()?;
                let asked = crate::mcp::egress::ask_warming(&manager, repo, &host)
                    .map_err(|error| {
                        tracing::warn!(repo = %repo.display(), %host, %error, "could not ask about egress");
                    })
                    .ok()
                    .flatten()?;
                if let crate::mcp::egress::Asked::Pending(row, true) = &asked {
                    let manager = manager.clone();
                    let row = row.clone();
                    tokio::spawn(async move { manager.approval_requested(&row).await });
                }
                return Some(crate::mcp::egress::warming_refusal(&host, &asked));
            }
            let session = grant.session_id.as_ref()?;
            let asked = match crate::mcp::egress::normalize_host(host) {
                Ok(host) => crate::mcp::egress::ask(&manager, session, &host, None, false)
                    .map_err(|error| {
                        tracing::warn!(%session, %host, %error, "could not ask about egress");
                    })
                    .ok(),
                Err(_) => None,
            };
            let first = {
                let mut seen = seen.lock().unwrap();
                // Bounded: a node that runs for months forgets, and at worst
                // says a refusal twice.
                if seen.len() > 10_000 {
                    seen.clear();
                }
                seen.insert((session.clone(), host.to_string()))
            };
            let approval_id = match &asked {
                Some(crate::mcp::egress::Asked::Pending(row, created)) => {
                    if *created {
                        let manager = manager.clone();
                        let row = row.clone();
                        tokio::spawn(async move { manager.approval_requested(&row).await });
                    }
                    Some(row.id.clone())
                }
                Some(crate::mcp::egress::Asked::Declined(row)) => Some(row.id.clone()),
                None => None,
            };
            if first {
                manager.record_event(
                    session,
                    ek::EGRESS_REFUSED,
                    json!({ "host": host, "approval_id": approval_id }),
                );
            }
            asked.map(|asked| crate::mcp::egress::refusal(host, &asked))
        })
    }

    /// Record an event on a session from outside the supervisor.
    /// Log a brokered call's step where its caller keeps its log.
    pub fn record_for(
        &self,
        ctx: &crate::mcp::CallContext,
        kind: &str,
        ref_id: Option<&str>,
        payload: serde_json::Value,
    ) {
        match ctx.session_id() {
            Some(session_id) => self.record(NewEvent {
                session_id: session_id.to_string(),
                work_item_id: None,
                kind: kind.to_string(),
                ref_id: ref_id.map(str::to_string),
                payload,
                at_ms: now_ms(),
                mono_ms: 0,
            }),
            None => self.record_external(&ctx.channel, ctx.lane(), kind, ref_id, payload),
        }
    }

    /// The harness processes that call under external lanes.
    pub fn liveness(&self) -> &external::Liveness {
        &self.liveness
    }

    /// Log what a session-less caller did on its channel.
    pub fn record_external(
        &self,
        channel: &str,
        lane: Option<&str>,
        kind: &str,
        ref_id: Option<&str>,
        payload: serde_json::Value,
    ) {
        match self.store.append_external_event(
            channel,
            &self.node_id,
            lane,
            kind,
            ref_id,
            &payload,
            now_ms(),
        ) {
            Ok(row) => self.bus.publish(Frame::ExternalEvent(Box::new(row))),
            Err(err) => tracing::error!(error = %err, "failed to persist external event"),
        }
    }

    pub fn record_event(&self, session_id: &str, kind: &str, payload: serde_json::Value) {
        self.record(NewEvent {
            session_id: session_id.to_string(),
            work_item_id: None,
            kind: kind.to_string(),
            ref_id: None,
            payload,
            at_ms: now_ms(),
            mono_ms: 0,
        });
    }

    /// The phase's artifact landed: end the session once its turn is over.
    pub async fn phase_done(&self, id: &str) {
        let _ = self
            .send(id, Command::EndAfterTurn(EndReason::PhaseDone))
            .await;
    }

    /// Resolve only configured, channel-authorized defaults. The node's model
    /// order is the adapter's order, so this stays deterministic without
    /// guessing at credentials that are not bound to this channel.
    fn resolve_default_model(
        &self,
        channel: &str,
        phase: Phase,
        bindings: &serde_json::Value,
    ) -> Result<(String, &'static str), SessionError> {
        for (value, source) in [
            (
                bindings["phases"][phase.as_str()]["model"].as_str(),
                "channel_phase",
            ),
            (bindings["model"].as_str(), "channel"),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                if self.model_usable(channel, value, bindings) {
                    return Ok((value.to_string(), source));
                }
                return Err(SessionError::ModelRequired);
            }
        }
        let models = self
            .store
            .get_node(&self.node_id)?
            .and_then(|node| node.models_json)
            .and_then(|json| serde_json::from_str::<Vec<crate::adapter::ModelOption>>(&json).ok())
            .unwrap_or_default();
        models
            .into_iter()
            .map(|model| model.value)
            .find(|model| self.model_usable(channel, model, bindings))
            .map(|model| (model, "node_catalogue"))
            .ok_or(SessionError::ModelRequired)
    }

    /// Harnesses use `provider/model` names where they can. A provider-less
    /// alias is adapter-owned and cannot be safely reverse-engineered here;
    /// it was already present in the node's successful catalogue.
    fn model_usable(&self, channel: &str, model: &str, bindings: &serde_json::Value) -> bool {
        let Some((provider_name, _)) = model.split_once('/') else {
            return true;
        };
        let Some(provider) = self.cfg.providers.get(provider_name) else {
            return true;
        };
        if let Some(allowed) = bindings["providers"].as_array() {
            if !allowed
                .iter()
                .any(|name| name.as_str() == Some(provider_name))
            {
                return false;
            }
        }
        let Ok(broker) = self.tools.broker.read() else {
            return false;
        };
        broker
            .inject_for_provider(
                provider_name,
                &provider.credential,
                channel,
                &self.node_id,
                &provider.shape,
            )
            .is_ok()
    }

    /// The model a spec will actually run on, and where it came from.
    ///
    /// An explicit model is checked against the channel's bound provider
    /// exactly as a resolved default is: naming one is a request, not an
    /// exemption, and a name the channel cannot authenticate would otherwise
    /// start a session that can only fail at its first turn. `create` and
    /// `preflight` both go through here, so both refuse it the same way.
    fn resolve_model(
        &self,
        spec: &NewSession,
        bindings: &serde_json::Value,
    ) -> Result<(String, &'static str), SessionError> {
        if spec.model.trim().is_empty() {
            self.resolve_default_model(&spec.channel, spec.phase, bindings)
        } else if self.model_usable(&spec.channel, &spec.model, bindings) {
            Ok((spec.model.clone(), "explicit"))
        } else {
            Err(SessionError::ModelRequired)
        }
    }

    /// The channel-archived and model-availability checks `create` runs,
    /// without any of its side effects. A caller that must materialize
    /// something expensive before `create` (a continuity transfer's
    /// workspace) checks this first, so a request `create` will reject
    /// outright never leaks that work.
    pub fn preflight(&self, spec: &NewSession) -> Result<(), SessionError> {
        let bindings = self.bindings(&spec.channel);
        if !bindings["archived"].is_null() {
            return Err(SessionError::ChannelArchived(spec.channel.clone()));
        }
        exhaustion::resolve(spec.on_exhaustion.as_ref(), &bindings)
            .map_err(SessionError::Rejected)?;
        let (model, _) = self.resolve_model(spec, &bindings)?;
        self.resolve_harness(&NewSession {
            model,
            ..spec.clone()
        })?;
        Ok(())
    }

    /// The policy revision the gate is running, as a launch manifest records
    /// it: a session's customization and the rules it ran under are one fact
    /// about how that session was configured.
    pub fn policy_version(&self) -> u32 {
        self.policy.read().version
    }

    /// A channel's bindings as JSON (`{}` when unbound or standalone).
    pub fn bindings(&self, channel: &str) -> serde_json::Value {
        self.store
            .channel_get(channel)
            .ok()
            .flatten()
            .and_then(|c| serde_json::from_str(&c.bindings_json).ok())
            .unwrap_or_else(|| json!({}))
    }

    /// Fence a live session without discarding its workspace or evidence.
    pub async fn pause(&self, id: &str, reason: String) -> Result<(), SessionError> {
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                id,
                Command::Pause {
                    source: supervisor::PauseSource::Operator,
                    reason: reason.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Pause {
                        session_id: id.to_string(),
                        reason,
                    },
                    false,
                )
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// Reopen a session that remains supervised on its owning node.
    pub async fn resume(&self, id: &str, reason: String) -> Result<(), SessionError> {
        if let Some(row) = self.store.get_session(id)? {
            Self::refuse_legacy(&row)?;
        }
        self.refuse_turn_while_restarting(id)?;
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                id,
                Command::Resume {
                    source: supervisor::PauseSource::Operator,
                    reason: reason.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Resume {
                        session_id: id.to_string(),
                        reason,
                    },
                    false,
                )
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// Change a session's token budget; `0` is none. Only the node that runs
    /// a session changes it, since that node's supervisor is what holds it to
    /// the budget. A session held at its budget, and by nothing else, is
    /// resumed once the new budget leaves it room; a running one just has
    /// more room.
    pub async fn set_budget(&self, id: &str, budget_tokens: i64) -> Result<(), SessionError> {
        if budget_tokens < 0 {
            return Err(SessionError::BadBudget);
        }
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        Self::refuse_legacy(&row)?;
        if row.node_id != self.node_id {
            return Err(SessionError::Rejected(format!(
                "session runs on {}; raise its budget on the node that runs it",
                self.store.node_display_name(&row.node_id)
            )));
        }
        let ended = || SessionError::Rejected(format!("session is {}", row.state));
        if SessionState::from_stored(&row.state).is_terminal() {
            return Err(ended());
        }
        // What the gateway's budget check counts: the settled turns plus
        // what the open one has spent so far. A budget at or under that is
        // refused on the next call, so it is refused here instead.
        let spent = row.tokens_used + row.open_turn_tokens;
        if budget_tokens > 0 && budget_tokens <= spent {
            return Err(SessionError::BudgetSpent(spent));
        }
        let changed = self.store.update_session_unless(
            id,
            SessionState::TERMINAL,
            SessionPatch {
                budget_tokens: Some(budget_tokens),
                ..Default::default()
            },
        )?;
        if !changed {
            return Err(ended());
        }
        self.record_event(
            id,
            ek::BUDGET_CHANGED,
            json!({ "from": row.budget_tokens, "to": budget_tokens, "tokens_used": row.tokens_used }),
        );
        let Some(row) = self.store.get_session(id)? else {
            return Err(SessionError::NotFound);
        };
        self.bus.publish(Frame::Session(Box::new(row.clone())));
        let held_at_budget = row.state == SessionState::Paused.as_str()
            && self
                .store
                .last_event_payload(id, ek::SESSION_PAUSED)?
                .is_some_and(|p| p["source"] == supervisor::PauseSource::Budget.as_str());
        if held_at_budget {
            self.resume(id, "operator raised the budget".into())
                .await
                .map_err(|e| {
                    SessionError::Rejected(format!(
                        "the budget is changed, but the session did not resume: {e}"
                    ))
                })?;
        }
        Ok(())
    }

    /// The gateway saw this session's provider refuse a call as exhausted.
    /// Decide under the session's policy — whether its fallback could take
    /// the work is a question for here, where the channel's bindings and the
    /// credentials are — and hand the decision to its supervisor, which
    /// records it and fences the session.
    pub async fn provider_exhausted(&self, id: &str, seen: crate::store::exhaustion::Exhausted) {
        let Ok(Some(row)) = self.store.get_session(id) else {
            return;
        };
        if !matches!(
            SessionState::from_stored(&row.state),
            SessionState::Running | SessionState::WaitingOnYou
        ) {
            return;
        }
        let choice = self
            .store
            .exhaustion(id)
            .ok()
            .flatten()
            .map(|r| r.choice())
            .unwrap_or_default();
        let usable = match choice.fallback.as_deref() {
            _ if choice.policy == exhaustion::Policy::Pause => Ok(()),
            None => Err("no fallback model is set".to_string()),
            Some(f) if f == row.model => Err(format!("the session is already on {f}")),
            Some(f) if !self.model_usable(&row.channel, f, &self.bindings(&row.channel)) => Err(
                format!("{f} has no provider or credential this channel may use"),
            ),
            Some(_) => Ok(()),
        };
        let decision = exhaustion::decide(&choice, seen.reset_ms, usable);
        let _ = self.send(id, Command::Exhausted { seen, decision }).await;
    }

    /// What would refuse an exhausted session going back to work now: the
    /// same checks that would refuse it starting, read again, because the
    /// wait may have been hours and anything could have changed in it.
    fn resume_check(&self, row: &SessionRow) -> Result<(), String> {
        let bindings = self.bindings(&row.channel);
        if !bindings["archived"].is_null() {
            return Err(format!("channel {} is archived", row.channel));
        }
        if let Ok(Some(node)) = self.store.get_node(&self.node_id) {
            if node.state != "ready" {
                return Err(format!(
                    "the node is not ready: {}",
                    node.failed_detail
                        .or(node.failed_check)
                        .unwrap_or_else(|| "boundary check failed".into())
                ));
            }
            if let Some(h) = node
                .harnesses()
                .into_iter()
                .find(|h| h.id == row.harness_id)
            {
                if h.mismatch() {
                    return Err(format!(
                        "the harness is {} where {} is pinned",
                        h.found.unwrap_or_default(),
                        h.pinned
                    ));
                }
            }
        }
        if !self.model_usable(&row.channel, &row.model, &bindings) {
            return Err(format!(
                "{} no longer has a provider or credential this channel may use",
                row.model
            ));
        }
        let ceiling = crate::metrics::ceiling(&self.store, &bindings, &row.channel);
        if ceiling.at() {
            return Err(ceiling.reason());
        }
        if row.budget_tokens > 0 && row.tokens_used >= row.budget_tokens {
            return Err("the session has spent its budget".into());
        }
        Ok(())
    }

    /// Act on every exhaustion that is due: wake a session whose provider's
    /// reset has come, carry one on to its fallback. Each only from its
    /// recorded safe boundary, and each settled exactly once. Run from the
    /// node's periodic tick.
    pub async fn wake_exhausted(&self) {
        if self.restart.draining() {
            return;
        }
        let Ok(due) = self.store.exhaustion_due(now_ms()) else {
            return;
        };
        for row in due {
            let Ok(Some(session)) = self.store.get_session(&row.session_id) else {
                continue;
            };
            if session.node_id != self.node_id {
                continue;
            }
            let current = SessionState::from_stored(&session.state);
            let ended = |reasons: &[state::EndReason]| {
                current.is_terminal()
                    && reasons
                        .iter()
                        .any(|r| session.end_reason.as_deref() == Some(r.as_str()))
            };
            let outcome = row.outcome.as_deref().unwrap_or_default();
            if outcome == exhaustion::FALLING_BACK {
                if !ended(&[
                    state::EndReason::ProviderExhausted,
                    state::EndReason::NodeRestart,
                ]) {
                    continue;
                }
                let fallback = row.fallback.clone().unwrap_or_default();
                let carried = self.store.carried_reviews(&session.id).unwrap_or_default();
                let note = exhausted_note(&session, Some(&fallback), &carried);
                let held = self.continuing().await;
                let result = self
                    .continue_work(
                        &held,
                        &session,
                        Some(fallback),
                        note,
                        told_reviews(&carried),
                        Some(row.choice()),
                    )
                    .await;
                self.settle_continuation(&row.session_id, result);
                continue;
            }
            // Waiting, and the provider's reset has come.
            if current == SessionState::Paused {
                if let Err(why) = self.resume_check(&session) {
                    self.hold_exhaustion(&row.session_id, &why);
                    continue;
                }
                let (ack, wait) = oneshot::channel();
                let sent = self
                    .send(
                        &row.session_id,
                        Command::Resume {
                            source: supervisor::PauseSource::Wake,
                            reason: "the provider's limit has reset".into(),
                            ack,
                        },
                    )
                    .await;
                let resumed = match sent {
                    Ok(()) => wait.await.unwrap_or_else(|_| Err("session stopped".into())),
                    Err(e) => Err(e.to_string()),
                };
                match resumed {
                    Ok(()) => {
                        if self
                            .store
                            .exhaustion_settle(&row.session_id, exhaustion::RESUMED, None, None)
                            .unwrap_or(false)
                        {
                            self.record_event(
                                &row.session_id,
                                ek::EXHAUSTION_WAKE,
                                json!({ "outcome": exhaustion::RESUMED }),
                            );
                        }
                        let _ = self
                            .prompt(
                                &row.session_id,
                                "The provider's limit has reset and the node resumed this \
                                 session. Your last turn was cut off when the provider \
                                 refused it; check what of it landed before carrying on."
                                    .into(),
                            )
                            .await;
                    }
                    // Not this session's moment (a turn still settling): the
                    // next tick tries again.
                    Err(e) => {
                        tracing::info!(session = %row.session_id, error = %e, "exhausted session not resumed yet");
                    }
                }
            } else if ended(&[state::EndReason::NodeRestart]) {
                // The node restarted while it waited: carry it on, on the same
                // model, once the reset has come.
                let carried = self.store.carried_reviews(&session.id).unwrap_or_default();
                let note = exhausted_note(&session, None, &carried);
                let held = self.continuing().await;
                let result = self
                    .continue_work(
                        &held,
                        &session,
                        None,
                        note,
                        told_reviews(&carried),
                        Some(row.choice()),
                    )
                    .await;
                self.settle_continuation(&row.session_id, result);
            } else if current.is_terminal()
                && self
                    .store
                    .exhaustion_settle(&row.session_id, exhaustion::ABANDONED, None, None)
                    .unwrap_or(false)
            {
                self.record_event(
                    &row.session_id,
                    ek::EXHAUSTION_WAKE,
                    json!({ "outcome": exhaustion::ABANDONED,
                            "note": "The session ended before its provider reset." }),
                );
            }
        }
    }

    fn settle_continuation(&self, id: &str, result: Result<SessionRow, SessionError>) {
        match result {
            Ok(next) => {
                if self
                    .store
                    .exhaustion_settle(id, exhaustion::CONTINUED, None, Some(&next.id))
                    .unwrap_or(false)
                {
                    self.record_event(
                        id,
                        ek::EXHAUSTION_WAKE,
                        json!({ "outcome": exhaustion::CONTINUED, "continued_by": next.id,
                                "model": next.model }),
                    );
                }
            }
            Err(e) => self.hold_exhaustion(id, &e.to_string()),
        }
    }

    fn hold_exhaustion(&self, id: &str, why: &str) {
        let note = format!("The node did not carry on: {why}.");
        if self.store.exhaustion_hold(id, &note).unwrap_or(false) {
            self.record_event(
                id,
                ek::EXHAUSTION_WAKE,
                json!({ "outcome": exhaustion::HELD, "note": note }),
            );
        }
    }

    /// Refuse every external harness on `channel`, or start allowing them
    /// again. External callers have no session, so this is their only fence.
    pub async fn set_external_stopped(
        &self,
        channel: &str,
        stopped: bool,
    ) -> Result<(), SessionError> {
        match self.channel_usable(channel) {
            Ok(()) | Err(SessionError::ChannelArchived(_)) => {}
            Err(e) => return Err(e),
        }
        write_external_stop(&self.store, &self.node_id, channel, stopped)?;
        Ok(())
    }

    /// Stop a live session. The operator's actual "Stop" action.
    pub async fn stop(&self, id: &str) -> Result<(), SessionError> {
        self.terminate(id).await
    }

    /// End one session; its row closes exactly as `stop` leaves it.
    pub async fn kill(&self, id: &str) -> Result<(), SessionError> {
        self.terminate(id).await
    }

    /// A startup row has no supervisor to command yet, so it is made
    /// terminal directly and startup observes that fence.
    async fn terminate(&self, id: &str) -> Result<(), SessionError> {
        // Fence dispatch synchronously before the asynchronous supervisor
        // teardown. A 200 must never leave broker access open.
        // Tracked so the stale-row fallback below can report the teardown
        // this call already performed instead of re-deriving eligibility
        // from a row this same call just made terminal.
        let mut already_closed = false;
        if let Some(row) = self.store.get_session(id)? {
            if row.node_id == self.node_id
                && row.state != SessionState::Starting.as_str()
                && !SessionState::from_stored(&row.state).is_terminal()
            {
                self.store.update_session(
                    id,
                    SessionPatch {
                        state: Some(SessionState::Closed.as_str().into()),
                        end_reason: Some(EndReason::KilledUser.as_str().into()),
                        ended_mono_ms: Some(0),
                        turn_active: Some(false),
                        ..Default::default()
                    },
                )?;
                already_closed = true;
                if let Ok(Some(row)) = self.store.get_session(id) {
                    self.bus.publish(Frame::Session(Box::new(row)));
                }
            }
        }
        match self.send(id, Command::Kill).await {
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Kill {
                        session_id: id.to_string(),
                    },
                    false,
                )
                .await
                .map(|_| ()),
            // No live supervisor, but this call already made the row
            // terminal and published it above: the teardown happened, so
            // report success rather than rejecting a row this call itself
            // just closed.
            Err(SessionError::Rejected(_)) if already_closed => Ok(()),
            Err(SessionError::Rejected(_)) => {
                let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
                if row.node_id != self.node_id
                    || (row.state != SessionState::Starting.as_str()
                        && row.state != SessionState::Paused.as_str())
                {
                    return Err(SessionError::Rejected(
                        "session is not running on this node".into(),
                    ));
                }
                self.store.update_session(
                    id,
                    SessionPatch {
                        state: Some(SessionState::Closed.as_str().into()),
                        end_reason: Some(EndReason::KilledUser.as_str().into()),
                        ended_mono_ms: Some(0),
                        turn_active: Some(false),
                        ..Default::default()
                    },
                )?;
                if let Some(container) = row.container_name {
                    let runner = self.backend.runner(Vec::new());
                    let _ = tokio::time::timeout(
                        CLEANUP_TIMEOUT,
                        crate::sidecars::stop_all(runner.as_ref(), &self.store, id),
                    )
                    .await;
                    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                }
                self.record(NewEvent {
                    session_id: id.to_string(),
                    work_item_id: None,
                    kind: ek::STATE.into(),
                    ref_id: None,
                    payload: json!({ "state": "closed", "end_reason": "killed_user" }),
                    at_ms: now_ms(),
                    mono_ms: 0,
                });
                if let Ok(Some(row)) = self.store.get_session(id) {
                    self.bus.publish(Frame::Session(Box::new(row)));
                }
                Ok(())
            }
            other => other,
        }
    }

    /// Carry on the sessions the node stopped under whose sandboxes it left
    /// running (`runner::reattach`): each is reopened on its own
    /// conversation, in the sandbox it ran in, and one that was in a turn
    /// is told to carry on with it. A sandbox no session can be carried on
    /// in is removed. Once, at start.
    pub async fn carry_on_after_restart(&self) {
        let rows = self.store.list_sessions(None).unwrap_or_default();
        for record in crate::runner::reattach::all() {
            let row = rows.iter().find(|s| {
                s.node_id == self.node_id && s.container_name.as_deref() == Some(&record.name)
            });
            let carried = match row {
                Some(row)
                    if row.state == SessionState::Closed.as_str()
                        && row.end_reason.as_deref()
                            == Some(state::EndReason::NodeRestart.as_str()) =>
                {
                    let ask = record
                        .in_turn
                        .then(|| Ask::Message(CARRY_ON_AFTER_RESTART.into()));
                    self.reopen_as(&row.id, ask).await.map(|_| ())
                }
                _ => Err(SessionError::Rejected(
                    "no session the node stopped under ran in it".into(),
                )),
            };
            match carried {
                Ok(()) => {
                    tracing::info!(sandbox = %record.name, "carrying a session on in the sandbox the node left running")
                }
                Err(error) => {
                    tracing::warn!(sandbox = %record.name, %error, "not carrying on in a sandbox the node left running; it is removed");
                    crate::runner::reattach::forget(&record.name);
                    self.backend
                        .reconcile(std::slice::from_ref(&record.name))
                        .await;
                }
            }
        }
    }

    /// Graceful shutdown: ask every live session to end and wait briefly.
    /// Containers are removed by each supervisor's teardown. The sessions end
    /// as `node_restart`, not `killed_user`: the operator stopped the node,
    /// not the work, and the session says so and offers to carry it on.
    pub async fn shutdown_all(&self) {
        let ids: Vec<String> = self.live.lock().await.keys().cloned().collect();
        for id in &ids {
            let _ = self
                .send(id, Command::End(state::EndReason::NodeRestart))
                .await;
        }
        for _ in 0..50 {
            if self.live.lock().await.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        tracing::warn!("some sessions did not stop in time");
    }
}

/// A restarted node owns no running harnesses: rows a previous process left
/// non-terminal are closed honestly, their open permission requests expired,
/// and their containers removed. Without this, an orphaned harness keeps the
/// credential store open and the model probe fails underneath it.
///
/// Only this node's sessions: a peer's mirrored rows are its to close.
pub async fn reconcile_after_restart(
    store: &Store,
    self_node_id: &str,
    backend: &dyn crate::boundary::Backend,
) -> Vec<String> {
    let mut cleaned = Vec::new();
    let Ok(sessions) = store.list_sessions(None) else {
        return cleaned;
    };
    for s in sessions {
        if s.node_id != self_node_id || state::SessionState::from_stored(&s.state).is_terminal() {
            continue;
        }
        // Already put away: nothing of it was running to be lost.
        if s.state == state::SessionState::Suspended.as_str() {
            continue;
        }
        for p in store.open_permissions().unwrap_or_default() {
            if p.session_id.as_deref() == Some(s.id.as_str()) {
                // Monotonic clocks do not survive a restart, so no meaningful
                // duration exists here. Resolve at the created reading (duration
                // 0 = not measured) rather than 0, which would go negative
                // against a nonzero created_mono_ms.
                let _ = store.resolve_permission(&p.id, "expired", None, p.created_mono_ms);
                let _ = store.append_event(&NewEvent {
                    session_id: s.id.clone(),
                    work_item_id: None,
                    kind: ek::PERMISSION_EXPIRED.into(),
                    ref_id: Some(p.id.clone()),
                    payload: json!({ "permission_id": p.id, "reason": "denied: node restarted" }),
                    at_ms: now_ms(),
                    mono_ms: 0,
                });
            }
        }
        if s.harness_id == external::HARNESS_ID {
            // External harnesses no longer attach as sessions. A row an older
            // node left open is closed, and one the operator had paused
            // becomes the channel's Stop, so the fence it held survives.
            if s.state == state::SessionState::Paused.as_str() {
                let _ = write_external_stop(store, self_node_id, &s.channel, true);
            }
            let _ = store.update_session(
                &s.id,
                SessionPatch {
                    state: Some(state::SessionState::Closed.as_str().into()),
                    end_reason: Some(state::EndReason::Detached.as_str().into()),
                    turn_active: Some(false),
                    ..Default::default()
                },
            );
            cleaned.push(s.id);
            continue;
        }
        if let Some(container) = &s.container_name {
            backend.reconcile(std::slice::from_ref(container)).await;
        }
        // Between turns, waiting only on the operator: nothing of it was
        // running to be lost. It is put away with what it waits on, and
        // carried on once that is decided (`Manager::continue_held`).
        let idle = s.turn_active == 0
            && matches!(
                state::SessionState::from_stored(&s.state),
                state::SessionState::Running | state::SessionState::WaitingOnYou
            );
        // The checks a restart interrupted are gone with the process; what
        // the store holds is what is still open.
        let open = if idle {
            waits::open(store, &Default::default(), &s.id)
        } else {
            Vec::new()
        };
        if !open.is_empty() {
            let waiting_on: Vec<_> = open.iter().map(waits::Wait::handle).collect();
            let _ = store.update_session(
                &s.id,
                SessionPatch {
                    state: Some(state::SessionState::Suspended.as_str().into()),
                    turn_active: Some(false),
                    ..Default::default()
                },
            );
            for (kind, payload) in [
                (
                    ek::SESSION_SUSPENDED,
                    json!({ "reason": "the node restarted while this session waited on the operator",
                            "waiting_on": waiting_on }),
                ),
                (
                    ek::STATE,
                    json!({ "state": "suspended", "end_reason": null, "waiting_on": waiting_on }),
                ),
            ] {
                let _ = store.append_event(&NewEvent {
                    session_id: s.id.clone(),
                    work_item_id: None,
                    kind: kind.into(),
                    ref_id: None,
                    payload,
                    at_ms: now_ms(),
                    mono_ms: 0,
                });
            }
            cleaned.push(s.id);
            continue;
        }
        let _ = store.update_session(
            &s.id,
            SessionPatch {
                state: Some(state::SessionState::Closed.as_str().into()),
                end_reason: Some(state::EndReason::NodeRestart.as_str().into()),
                turn_active: Some(false),
                ..Default::default()
            },
        );
        let _ = store.append_event(&NewEvent {
            session_id: s.id.clone(),
            work_item_id: None,
            kind: ek::STATE.into(),
            ref_id: None,
            payload: json!({ "state": "closed", "end_reason": "node_restart", "reason": "node restarted" }),
            at_ms: now_ms(),
            mono_ms: 0,
        });
        cleaned.push(s.id);
    }
    cleaned
}

/// What the `worktree` event and the orientation say of a session started
/// from a clone that could not be fetched: when the default branch it
/// started from was last fetched, and why this fetch failed.
fn stale_note(error: &str, fetched_ms: Option<i64>) -> String {
    let when = match fetched_ms {
        Some(at) => exhaustion::utc(at),
        None => "an unknown time".to_string(),
    };
    format!("stale: last fetched at {when}; the fetch failed: {error}")
}

/// A URL-safe random token. Not a session id: ids appear in logs and the
/// interface, and a capability must not be guessable from something displayed.
fn mint_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Comparison that does not leak how much of a token matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The durable fence behind an external Stop. Stopping also retires the
/// channel-wide pause an older node may have left; starting clears both.
fn write_external_stop(
    store: &Store,
    node_id: &str,
    channel: &str,
    stopped: bool,
) -> Result<(), SessionError> {
    if store.channel_get(channel)?.is_none() {
        // Materialize both standalone defaults together; creating one
        // otherwise makes the other disappear from the synthesized list.
        for name in crate::http::api::DEFAULT_CHANNELS {
            store.channel_put(name, &[], "{}")?;
            store.node_channel_add(node_id, name)?;
        }
    }
    let (keyring, mut bindings) = match store.channel_get(channel)? {
        Some(channel) => (
            channel.keyring,
            serde_json::from_str(&channel.bindings_json).unwrap_or_else(|_| json!({})),
        ),
        None => (Vec::new(), json!({})),
    };
    if let Some(map) = bindings.as_object_mut() {
        map.remove("external_paused");
        if stopped {
            map.insert("external_stopped".into(), json!(true));
        } else {
            map.remove("external_stopped");
        }
    }
    store.channel_put(
        channel,
        &keyring,
        &serde_json::to_string(&bindings)
            .map_err(|error| SessionError::Rejected(error.to_string()))?,
    )?;
    Ok(())
}

/// The runner a session's harness starts in, and the environment its cache
/// adds. A session in its repository's image runs there, on any runtime, with
/// its cache at `/cache` and the variables that send package managers and
/// builds into it: the agent installs and builds into the session cache too,
/// so the next session on the repository finds what this one compiled. One on
/// the stock base has no cache and gets neither.
pub fn session_runner(
    backend: &dyn crate::boundary::Backend,
    harness_id: &str,
    image: Option<&str>,
    cache: Option<String>,
    mut mounts: Vec<crate::runner::Mount>,
) -> (Arc<dyn Runner>, Vec<(String, String)>) {
    let Some(image) = image else {
        return (backend.runner_for(harness_id, mounts), Vec::new());
    };
    let mut cache_env = Vec::new();
    if let Some(cache) = cache {
        mounts.push(crate::runner::Mount::volume(cache, "/cache", false));
        cache_env = crate::environment::cache_env();
        cache_env.extend(crate::environment::session_build_env());
    }
    (backend.runner_in(harness_id, image, mounts), cache_env)
}

/// What a session's harness is given as its environment: the wiring's base
/// URLs and Git's settings in the open, and its secrets as secret
/// environment, which never reaches a pod spec or an argv. Those are the
/// wiring's token, which is the session's credential at the gateway and the
/// MCP server, and the egress grant's proxy URL, which carries its password.
pub fn harness_environment(
    wiring: &crate::gateway::model::Wiring,
    egress: Option<&crate::boundary::EgressGrant>,
    home: &str,
) -> (Vec<(String, String)>, crate::runner::SecretEnv) {
    let mut env = wiring.plain_env();
    env.extend([
        ("GIT_CONFIG_GLOBAL".into(), format!("{home}/.gitconfig")),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
    ]);
    let mut secret = wiring.secret_env();
    if let Some(egress) = egress {
        secret.extend(egress.env().iter().cloned());
    }
    (env, secret)
}

/// Run the repository's `prepare` on a session's workspace volume before its
/// harness starts, in the repository's image with the session's `cache`
/// writable and the entry's own egress for exactly as long as it runs: the
/// steps a candidate is prepared with before its checks. `None` when the
/// entry prepares nothing; `NotRun`, with `cache`'s reason, when the session
/// has no cache to prepare against because it is not in its repository's
/// image. Egress the runtime cannot open (`Backend::egress_grant`) fails the
/// preparation with the runtime's reason, as it fails a check's. A failure is the
/// outcome, not an error: the session starts anyway, since it is the one that
/// can fix what broke. The entry's own time limit bounds it, as it bounds a
/// check's preparation, and `cancel` stops it sooner.
/// What a session's metric series are labelled with (`telemetry`).
fn session_series(
    store: &Store,
    id: &str,
    spec: &NewSession,
    harness: &str,
) -> crate::telemetry::SessionSeries {
    crate::telemetry::SessionSeries::new(
        spec.phase.as_str(),
        harness,
        crate::telemetry::session_repo(store, id).as_deref(),
        Some(&spec.channel),
    )
}

/// The prepared snapshot a session's sandbox starts from, the commit it was
/// prepared at (`sandbox_images::prepared_from`), and the commit the
/// session's workspace was seeded at (`None` for a resumed workspace).
#[derive(Debug, Clone, Copy)]
pub struct FromSnapshot<'a> {
    pub snapshot: &'a str,
    pub prepared_at: Option<&'a str>,
    pub base: Option<&'a str>,
}

pub async fn prepare_session_workspace(
    backend: &dyn crate::boundary::Backend,
    environment: &crate::environment::RepoEnvironment,
    (workspace, cache): (&str, Result<&str, String>),
    runner_name: &str,
    cancel: &dyn crate::review::checks::Cancel,
    from: Option<FromSnapshot<'_>>,
) -> Option<crate::environment::WorkspacePreparation> {
    use crate::environment::PrepareOutcome;
    use crate::review::checks::Stopped;
    if environment.prepare.is_empty() {
        return None;
    }
    let begun = Instant::now();
    let outcome = match cache {
        Err(why) => PrepareOutcome::NotRun(why),
        // What `prepare` would install at this commit is what the snapshot
        // already holds under the workspace.
        Ok(_)
            if from.is_some_and(|from| {
                backend.workspace_over_image()
                    && crate::sandbox_images::skips_prepare(from.prepared_at, from.base)
            }) =>
        {
            PrepareOutcome::FromSnapshot {
                commit: from
                    .and_then(|from| from.prepared_at)
                    .unwrap_or_default()
                    .to_string(),
            }
        }
        Ok(cache) => {
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(environment.timeout_secs);
            let prepared = match (from, backend.preparer()) {
                (Some(from), Some(preparer)) => {
                    prepare_in_snapshot(
                        backend,
                        &preparer,
                        environment,
                        (workspace, from.snapshot),
                        runner_name,
                        deadline,
                        cancel,
                    )
                    .await
                }
                _ => {
                    let runner = backend.runner(Vec::new());
                    crate::review::checks::run_preparation(
                        backend,
                        runner.as_ref(),
                        environment,
                        (workspace, cache),
                        runner_name,
                        deadline,
                        cancel,
                        "the session starts without it",
                        crate::telemetry::Purpose::Session,
                    )
                    .await
                }
            };
            match prepared {
                Ok(()) => PrepareOutcome::Prepared,
                Err(Stopped::Unprepared(failure)) => PrepareOutcome::Failed(failure.to_string()),
                Err(Stopped::TimedOut) => PrepareOutcome::TimedOut {
                    after_secs: environment.timeout_secs,
                },
                Err(Stopped::Cancelled { reason, .. }) => PrepareOutcome::Cancelled(reason),
            }
        }
    };
    Some(crate::environment::WorkspacePreparation {
        commands: environment.prepare.clone(),
        outcome,
        elapsed_ms: begun.elapsed().as_millis() as u64,
    })
}

/// A session's `prepare` at a commit other than its snapshot's: every step
/// in one sandbox from the snapshot, over the `/work` and `/cache` its
/// preparation left, and the workspace taken back whole once the steps are
/// done. Neither the node's cache nor a step's output crosses between steps.
async fn prepare_in_snapshot(
    backend: &dyn crate::boundary::Backend,
    preparer: &crate::sandbox_images::Preparer,
    environment: &crate::environment::RepoEnvironment,
    (workspace, snapshot): (&str, &str),
    runner_name: &str,
    deadline: tokio::time::Instant,
    cancel: &dyn crate::review::checks::Cancel,
) -> Result<(), crate::review::checks::Stopped> {
    use crate::review::checks::{PrepareFailure, Stopped};
    const UNPREPARED: &str = "the session starts without it";
    let held = crate::sandbox_images::Held::for_session(
        preparer,
        &format!("{runner_name}-prep"),
        snapshot,
        workspace,
    )
    .await
    .map_err(|error| {
        Stopped::Unprepared(PrepareFailure::said(format!(
            "could not start the preparation sandbox: {error}; {UNPREPARED}."
        )))
    })?;
    let prepared = crate::review::checks::run_preparation(
        backend,
        &held,
        environment,
        (workspace, ""),
        runner_name,
        deadline,
        cancel,
        UNPREPARED,
        crate::telemetry::Purpose::Session,
    )
    .await;
    let taken = held.take_back().await;
    match (prepared, taken) {
        (Ok(()), Err(error)) => Err(Stopped::Unprepared(PrepareFailure::said(format!(
            "{error}; {UNPREPARED}."
        )))),
        (prepared, _) => prepared,
    }
}

/// A starting session's preparation is no longer wanted once the session
/// has left `starting`: the operator stopped it.
struct Stopping<'a> {
    store: &'a Store,
    id: &'a str,
}

impl crate::review::checks::Cancel for Stopping<'_> {
    fn cancelled(&self) -> Option<String> {
        let starting = self
            .store
            .get_session(self.id)
            .ok()
            .flatten()
            .is_some_and(|row| row.state == SessionState::Starting.as_str());
        (!starting).then(|| "the session was stopped".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(plan: Option<&str>) -> tracon_sync::work::WorkItem {
        tracon_sync::work::WorkItem {
            id: "0123456789abcdef".into(),
            channel: "personal".into(),
            project_id: None,
            title: "Add the ledger".into(),
            body: String::new(),
            state: "open".into(),
            priority: 0,
            deps: vec![],
            discovered_from: None,
            discovered_by_session: None,
            discovered_by_lane: None,
            phase_plan_slug: plan.map(str::to_string),
            closed_by_session: None,
            created_ms: 0,
            updated_ms: 0,
            ..Default::default()
        }
    }

    #[test]
    fn a_plain_session_has_no_kickoff() {
        assert_eq!(kickoff(Phase::Execute, None, None), None);
    }

    #[test]
    fn an_execute_session_without_a_plan_starts_on_the_item_alone() {
        let text = kickoff(Phase::Execute, Some(&item(None)), None).unwrap();
        let name = crate::names::task("0123456789abcdef");
        assert!(
            text.contains(&format!("**Add the ledger** (`{name}`)")),
            "{text}"
        );
        assert!(!text.contains("plan"), "{text}");
    }

    #[test]
    fn a_review_session_is_told_which_review_and_how_to_answer() {
        let text = kickoff(Phase::Review, None, Some("r-1")).unwrap();
        assert!(text.contains("`r-1`"), "{text}");
        assert!(text.contains("`review_verdict`"), "{text}");
        // The workspace is the candidate's files, not a checkout.
        assert!(text.contains(crate::mcp::review::REVIEW_DIFF), "{text}");
        assert!(!text.contains("checked out"), "{text}");
    }

    fn preparing(prepare: &[&str], timeout_secs: u64) -> crate::environment::RepoEnvironment {
        crate::environment::RepoEnvironment {
            image: "local".into(),
            image_source: "harness image",
            checks: Vec::new(),
            timeout_secs,
            prepare: prepare.iter().map(|command| command.to_string()).collect(),
            egress: Vec::new(),
            session_egress: Vec::new(),
            cache_volume: "tracon-cache-base-unused".into(),
            repo: None,
        }
    }

    /// A workspace and a cache volume `LocalBackend` resolves to directories.
    fn volumes() -> (String, String) {
        let id = uuid::Uuid::now_v7().simple().to_string();
        let (work, cache) = (
            format!("tracon-workspace-{id}"),
            format!("tracon-cache-r-{id}"),
        );
        for volume in [&work, &cache] {
            std::fs::create_dir_all(crate::runner::local::local_runtime_path(volume)).unwrap();
        }
        (work, cache)
    }

    struct Stopped;

    impl crate::review::checks::Cancel for Stopped {
        fn cancelled(&self) -> Option<String> {
            Some("the session was stopped".into())
        }
    }

    async fn prepare(
        environment: &crate::environment::RepoEnvironment,
        (work, cache): (&str, Result<&str, String>),
        cancel: &dyn crate::review::checks::Cancel,
    ) -> Option<crate::environment::WorkspacePreparation> {
        prepare_session_workspace(
            &crate::runner::local::LocalBackend,
            environment,
            (work, cache),
            "tracon-s-test",
            cancel,
            None,
        )
        .await
    }

    /// `LocalBackend`, but with a workspace sent over its image's `/work`,
    /// as a node with files over the protocol sends it.
    struct OverImage;

    #[async_trait::async_trait]
    impl crate::boundary::Backend for OverImage {
        fn kind(&self) -> &'static str {
            "local"
        }
        async fn setup(
            &self,
            _cfg: &Config,
            _rebuild: bool,
        ) -> Result<(), crate::boundary::BoundaryError> {
            Ok(())
        }
        async fn check_all(&self, _cfg: &Config, _deep: bool) -> crate::boundary::BoundaryReport {
            crate::boundary::BoundaryReport { checks: Vec::new() }
        }
        fn runner(&self, _extra: Vec<crate::runner::Mount>) -> Arc<dyn crate::runner::Runner> {
            Arc::new(crate::runner::local::LocalRunner)
        }
        fn runner_for(
            &self,
            _harness: &str,
            _extra: Vec<crate::runner::Mount>,
        ) -> Arc<dyn crate::runner::Runner> {
            Arc::new(crate::runner::local::LocalRunner)
        }
        fn harness_host(&self) -> String {
            "localhost".into()
        }
        fn harness_home(&self) -> String {
            materialize::PODMAN_HARNESS_HOME.into()
        }
        async fn reconcile(&self, _names: &[String]) {}
        async fn import_volume(
            &self,
            volume: &str,
            source: &std::path::Path,
        ) -> Result<(), crate::boundary::BoundaryError> {
            crate::runner::local::LocalBackend
                .import_volume(volume, source)
                .await
        }
        async fn export_volume(
            &self,
            volume: &str,
            destination: &std::path::Path,
        ) -> Result<(), crate::boundary::BoundaryError> {
            crate::runner::local::LocalBackend
                .export_volume(volume, destination)
                .await
        }
        fn workspace_over_image(&self) -> bool {
            true
        }
    }

    /// A session from its snapshot skips `prepare` only at the commit the
    /// snapshot was prepared from, and only where its workspace is sent over
    /// the snapshot's `/work`; at another commit, from a stale snapshot (no
    /// commit to compare), or with the workspace mounted in place of the
    /// image's, it prepares as before.
    #[tokio::test]
    async fn a_session_at_its_snapshots_commit_skips_prepare() {
        use crate::environment::PrepareOutcome;
        let environment = preparing(&["touch prepared"], 60);
        let at = |prepared_at: Option<&'static str>, base: Option<&'static str>| {
            Some(FromSnapshot {
                snapshot: "localhost/tracon-snapshot-app@sha256:aa",
                prepared_at,
                base,
            })
        };
        let run = |backend: &'static dyn crate::boundary::Backend, from| {
            let environment = environment.clone();
            async move {
                let (work, cache) = volumes();
                let done = prepare_session_workspace(
                    backend,
                    &environment,
                    (&work, Ok(&cache)),
                    "tracon-s-test",
                    &crate::review::checks::RunToCompletion,
                    from,
                )
                .await
                .unwrap();
                let ran = crate::runner::local::local_runtime_path(&work)
                    .join("prepared")
                    .exists();
                (done, ran)
            }
        };

        let (done, ran) = run(&OverImage, at(Some("c1"), Some("c1"))).await;
        assert_eq!(
            done.outcome,
            PrepareOutcome::FromSnapshot {
                commit: "c1".into()
            }
        );
        assert!(!ran, "nothing ran");
        let payload = done.payload();
        assert_eq!(payload["outcome"], "from_snapshot");
        assert_eq!(payload["commit"], "c1");
        assert_eq!(payload["commands"][0], "touch prepared");

        for (backend, from) in [
            (
                &OverImage as &dyn crate::boundary::Backend,
                at(Some("c1"), Some("c2")),
            ),
            (&OverImage, at(None, Some("c1"))),
            (&OverImage, at(Some("c1"), None)),
            (&OverImage, None),
            (
                &crate::runner::local::LocalBackend,
                at(Some("c1"), Some("c1")),
            ),
        ] {
            let (done, ran) = run(backend, from).await;
            assert_eq!(done.outcome, PrepareOutcome::Prepared, "{from:?}");
            assert!(ran, "{from:?}");
        }
    }

    #[tokio::test]
    async fn the_repositorys_prepare_runs_on_the_session_workspace() {
        use crate::environment::PrepareOutcome;
        let (work, cache) = volumes();
        let environment = preparing(&["touch prepared", "echo second > second"], 60);
        let done = prepare(
            &environment,
            (&work, Ok(&cache)),
            &crate::review::checks::RunToCompletion,
        )
        .await
        .unwrap();
        assert_eq!(done.outcome, PrepareOutcome::Prepared);
        assert_eq!(done.commands, ["touch prepared", "echo second > second"]);
        let dir = crate::runner::local::local_runtime_path(&work);
        assert!(dir.join("prepared").is_file() && dir.join("second").is_file());
        let payload = done.payload();
        assert_eq!(payload["outcome"], "prepared");
        assert_eq!(payload["commands"][1], "echo second > second");
        assert!(payload["ms"].is_u64());
        assert!(payload.get("detail").is_none());
    }

    #[tokio::test]
    async fn a_failed_prepare_is_an_outcome_that_says_what_failed() {
        use crate::environment::PrepareOutcome;
        let (work, cache) = volumes();
        let environment = preparing(&["echo boom >&2; exit 3", "touch never"], 60);
        let done = prepare(
            &environment,
            (&work, Ok(&cache)),
            &crate::review::checks::RunToCompletion,
        )
        .await
        .unwrap();
        let PrepareOutcome::Failed(detail) = &done.outcome else {
            panic!("{done:?}");
        };
        assert!(detail.contains("(exit 3)"), "{detail}");
        assert!(detail.contains("the session starts without it"), "{detail}");
        assert!(!detail.contains("the check was not run"), "{detail}");
        assert!(detail.contains("boom"), "{detail}");
        // A failed step ends the preparation: the next one never ran.
        assert!(!crate::runner::local::local_runtime_path(&work)
            .join("never")
            .exists());
        assert_eq!(done.payload()["outcome"], "failed");
        assert_eq!(done.payload()["detail"], detail.as_str());
    }

    #[tokio::test]
    async fn a_prepare_that_hangs_gives_up_at_the_entrys_timeout() {
        use crate::environment::PrepareOutcome;
        let (work, cache) = volumes();
        let environment = preparing(&["sleep 30"], 1);
        let begun = Instant::now();
        let done = prepare(
            &environment,
            (&work, Ok(&cache)),
            &crate::review::checks::RunToCompletion,
        )
        .await
        .unwrap();
        assert_eq!(done.outcome, PrepareOutcome::TimedOut { after_secs: 1 });
        assert!(
            begun.elapsed() < Duration::from_secs(15),
            "{:?}",
            begun.elapsed()
        );
        assert_eq!(done.payload()["timeout_secs"], 1);
    }

    #[tokio::test]
    async fn stopping_the_session_stops_its_preparation() {
        use crate::environment::PrepareOutcome;
        let (work, cache) = volumes();
        let environment = preparing(&["sleep 30"], 600);
        let begun = Instant::now();
        let done = prepare(&environment, (&work, Ok(&cache)), &Stopped)
            .await
            .unwrap();
        assert_eq!(
            done.outcome,
            PrepareOutcome::Cancelled("the session was stopped".into())
        );
        assert!(
            begun.elapsed() < Duration::from_secs(15),
            "{:?}",
            begun.elapsed()
        );
    }

    #[tokio::test]
    async fn nothing_runs_without_commands_or_without_a_cache() {
        use crate::environment::PrepareOutcome;
        let (work, _) = volumes();
        let none = prepare(
            &preparing(&[], 60),
            (&work, Err("no image".into())),
            &crate::review::checks::RunToCompletion,
        )
        .await;
        assert_eq!(none, None);
        let done = prepare(
            &preparing(&["touch prepared"], 60),
            (
                &work,
                Err(
                    "the session runs in the harness's own image, not its repository's: \
                     the kubernetes runtime cannot build images"
                        .into(),
                ),
            ),
            &crate::review::checks::RunToCompletion,
        )
        .await
        .unwrap();
        assert!(
            matches!(done.outcome, PrepareOutcome::NotRun(_)),
            "{done:?}"
        );
        assert!(!crate::runner::local::local_runtime_path(&work)
            .join("prepared")
            .exists());
        assert_eq!(done.payload()["outcome"], "not_run");
        // The reason is the caller's, word for word: what is still missing.
        assert!(done.payload()["reason"]
            .as_str()
            .unwrap()
            .ends_with("the kubernetes runtime cannot build images"));
    }
}
