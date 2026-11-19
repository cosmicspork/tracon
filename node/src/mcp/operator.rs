use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, Duration};

use crate::{
    config::Config,
    mcp::{
        review::caller_owns,
        wait::{wait_secs, MAX_WAIT_SECS},
        CallContext,
    },
    notify,
    store::{now_ms, IssueDraftRow, OperatorQuestionRow, Store},
};

pub const ASK: &str = "ask_operator";
pub const NOTIFY: &str = "notify_operator";
pub const REPORT: &str = "report_issue";
pub const REPORT_STATUS: &str = "issue_report_status";
pub const QUESTION_STATUS: &str = "question_status";
/// `ask_operator`'s question and choices.
const MAX_TEXT: usize = 8 * 1024;

// `report_issue`'s limits. Every one is counted in characters (Unicode scalar
// values, `str::chars().count()`), as GitHub counts its limits and as JSON
// Schema's `maxLength` counts, so the schema the agent reads and the check
// the node makes agree. The schema and the description are generated from
// these constants.

/// GitHub refuses an issue title longer than this. Not documented by GitHub:
/// observed by creating issues (<https://github.com/dead-claudia/github-limits>).
pub const GITHUB_ISSUE_TITLE_MAX_CHARS: usize = 256;
/// GitHub refuses an issue body longer than this ("body is too long (maximum
/// is 65536 characters)"). Not documented by GitHub: observed in 422 errors
/// (<https://github.com/orgs/community/discussions/27190>) and measured in
/// code points by <https://github.com/dead-claudia/github-limits>.
pub const GITHUB_ISSUE_BODY_MAX_CHARS: usize = 65_536;
/// Room left between our cap and GitHub's, because GitHub's figure is
/// observed rather than documented and GitHub may count slightly differently
/// (CRLF normalisation, for one).
pub const REPORT_BODY_MARGIN_CHARS: usize = 5_536;
/// The binding limit: the issue body exactly as it is published — every
/// field, the section headings, the attachments block and the draft marker.
pub const REPORT_BODY_MAX_CHARS: usize = 60_000;
/// One text field may use the whole budget, so a lopsided report (a short
/// `expected`, a long `diagnosis`) fits. It exists so the schema states a
/// bound and absurd input is refused before it is scrubbed.
pub const REPORT_FIELD_MAX_CHARS: usize = REPORT_BODY_MAX_CHARS;
/// One attachment's content.
pub const REPORT_ATTACHMENT_MAX_CHARS: usize = 8_192;
/// One attachment's name.
pub const REPORT_ATTACHMENT_NAME_MAX_CHARS: usize = 256;
pub const MAX_ATTACHMENTS: usize = 3;
const _: () =
    assert!(REPORT_BODY_MAX_CHARS + REPORT_BODY_MARGIN_CHARS <= GITHUB_ISSUE_BODY_MAX_CHARS);

/// `report_issue`'s text fields, in the order they are published, with the
/// heading each is published under. The first two are required.
const REPORT_SECTIONS: [(&str, &str); 7] = [
    ("expected", "Expected behavior"),
    ("actual", "Actual behavior"),
    ("reproduction", "Reproduction"),
    ("versions", "Versions"),
    ("errors", "Relevant errors"),
    ("recovery", "Attempted recovery"),
    ("diagnosis", "Agent diagnosis (not observed evidence)"),
];

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": ASK,
            "description": format!(
                "Ask the operator a free-text question, optionally offering explicit \
                 choices. The operator can always write free text as well as, or instead of, \
                 picking a choice, so offer only real options and never an \"Other\" choice to \
                 make room for an explanation. An answer is `{{choice, text}}`: `choice` is the \
                 offered choice they picked and `text` what they wrote; either may be absent, \
                 never both. Waits up to {MAX_WAIT_SECS} seconds for the answer and returns \
                 `answered` with it, `cancelled` if the operator withdrew the question, or \
                 `unanswered` with `still_waiting: true` and a `question_id` when they have not \
                 answered yet. The question stays in their queue either way: call \
                 {QUESTION_STATUS} with the question_id to keep waiting. A stable request_id \
                 recovers the same question after a reconnect or restart. An answer never \
                 grants access."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": { "type": "string", "maxLength": 128 },
                    "question": { "type": "string" },
                    "choices": { "type": "array", "items": { "type": "string" }, "maxItems": 20 },
                    "wait_secs": wait_secs_schema(),
                },
                "required": ["request_id", "question"],
            },
        }),
        json!({
            "name": QUESTION_STATUS,
            "description": format!(
                "Wait for the answer to questions you asked with {ASK}. Pass `question_id`, or \
                 `question_ids` to wait on several: the call returns as soon as any of them is \
                 answered or cancelled, with every id's state — `unanswered`, `answered` with \
                 the `answer` (`{{choice, text}}`, either of which may be absent), or \
                 `cancelled` when the operator withdrew it without answering. `still_waiting` \
                 is true when nothing was decided. {}",
                 super::wait::pointer()
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question_id": { "type": "string" },
                    "question_ids": { "type": "array", "items": { "type": "string" } },
                    "wait_secs": wait_secs_schema(),
                },
            },
        }),
        json!({"name": NOTIFY, "description":"Intentionally send an OS/PWA notification to configured operator devices. Without `node_ids`, every node of this channel delivers it to the devices subscribed there. A push-service attempt is not evidence a human read it.", "inputSchema":{"type":"object","properties":{"title":{"type":"string"},"message":{"type":"string"},"path":{"type":"string"},"device_ids":{"type":"array","items":{"type":"string"}},"node_ids":{"type":"array","items":{"type":"string"}}},"required":["title","message"]}}),
        report_definition(),
        json!({
            "name": REPORT_STATUS,
            "description": format!(
                "What became of issues you drafted with {REPORT}. Pass `issue_id`, or \
                 `issue_ids` to wait on several: the call returns as soon as any of them is \
                 decided, with every id's state — `draft` or `publishing` while the operator \
                 has not decided, `published` with the GitHub issue `number`, `url` and \
                 `title`, `discarded` with the operator's `reason`, or `uncertain` when \
                 publication may or may not have reached GitHub. `still_waiting` is true when \
                 nothing was decided. A draft is not a wait: ending your turn does not hold \
                 the session for it. {}",
                 super::wait::pointer()
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "issue_id": { "type": "string" },
                    "issue_ids": { "type": "array", "items": { "type": "string" } },
                    "wait_secs": wait_secs_schema(),
                },
            },
        }),
    ]
}

fn report_definition() -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "title".into(),
        json!({ "type": "string", "maxLength": GITHUB_ISSUE_TITLE_MAX_CHARS }),
    );
    for (key, _) in REPORT_SECTIONS {
        properties.insert(
            key.into(),
            json!({ "type": "string", "maxLength": REPORT_FIELD_MAX_CHARS }),
        );
    }
    properties.insert(
        "attachments".into(),
        json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "maxLength": REPORT_ATTACHMENT_NAME_MAX_CHARS },
                    "content": { "type": "string", "maxLength": REPORT_ATTACHMENT_MAX_CHARS },
                },
                "required": ["name", "content"],
            },
            "maxItems": MAX_ATTACHMENTS,
        }),
    );
    json!({
        "name": REPORT,
        "description": format!(
            "Draft a report about tracon, its environments, tools, or harness integration. It \
             never pauses execution or uploads repository files/transcripts. The operator \
             inspects and authorizes publication separately. Limits, in characters: the title \
             at most {}; all fields, with their section headings and the attachments \
             (published as pretty-printed JSON), must fit in {} characters once published, \
             and any single field may use all of it; at most {MAX_ATTACHMENTS} attachments, \
             each with a name of at most {} and content of at most {}. Redacting \
             secret-looking lines can lengthen a field.",
            thousands(GITHUB_ISSUE_TITLE_MAX_CHARS),
            thousands(REPORT_BODY_MAX_CHARS),
            thousands(REPORT_ATTACHMENT_NAME_MAX_CHARS),
            thousands(REPORT_ATTACHMENT_MAX_CHARS),
        ),
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": ["title", "expected", "actual"],
        },
    })
}

fn wait_secs_schema() -> Value {
    json!({
        "type": "integer",
        "description": super::wait::wait_secs_description(),
    })
}

pub async fn call(
    store: &Arc<Store>,
    manager: &crate::session::Manager,
    cfg: &Config,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        ASK => ask(store, manager, ctx, args).await,
        NOTIFY => notify_operator(store, manager, cfg, ctx, args).await,
        REPORT => report(store, ctx, args),
        REPORT_STATUS => report_status(store, ctx, args).await,
        QUESTION_STATUS => {
            let ids = ids(args, "question_id", "question_ids")?;
            question_status(store, manager, ctx, &ids, args).await
        }
        _ => Err(format!("no operator tool named {name}")),
    }
}

async fn ask(
    store: &Arc<Store>,
    manager: &crate::session::Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let request_key = text(args, "request_id", true)?;
    if request_key.len() > 128
        || !request_key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(
            "request_id must be up to 128 ASCII letters, digits, dots, dashes, or underscores"
                .into(),
        );
    }
    let prompt = text(args, "question", true)?;
    let choices = string_array(args, "choices", 20, MAX_TEXT)?;
    let choices_json = serde_json::to_string(&choices).unwrap();
    let existing = match ctx.session_id() {
        Some(session_id) => store.operator_question_for_request(session_id, &request_key),
        None => {
            store.operator_question_for_channel_request(&ctx.channel, &ctx.node_id, &request_key)
        }
    }
    .map_err(|e| e.to_string())?;
    let row = match existing {
        Some(existing) => {
            if existing.channel != ctx.channel
                || existing.node_id != ctx.node_id
                || existing.prompt != prompt
                || existing.choices_json != choices_json
            {
                return Err("request_id is already bound to a different question".into());
            }
            existing
        }
        None => {
            let row = OperatorQuestionRow {
                id: uuid::Uuid::now_v7().to_string(),
                session_id: ctx.session_id().map(str::to_string),
                channel: ctx.channel.clone(),
                node_id: ctx.node_id.clone(),
                request_key: Some(request_key),
                prompt,
                choices_json,
                state: "unanswered".into(),
                answer_json: None,
                created_ms: now_ms(),
                answered_ms: None,
            };
            let stored = store
                .insert_operator_question(&row)
                .map_err(|e| e.to_string())?;
            if stored.channel != row.channel
                || stored.node_id != row.node_id
                || stored.prompt != row.prompt
                || stored.choices_json != row.choices_json
            {
                return Err("request_id is already bound to a different question".into());
            }
            if stored.id == row.id {
                publish_questions(store, manager.bus());
            }
            stored
        }
    };
    question_status(store, manager, ctx, &[row.id], args).await
}

/// Tell the notifier and the mesh which questions are open, after one is
/// asked, answered or withdrawn here. The notifier diffs and debounces it;
/// the mesh carries this node's own, so a peer showing them learns when one
/// is no longer open.
pub(crate) fn publish_questions(store: &Store, bus: &crate::stream::Bus) {
    if let Ok(waiting) = store.open_operator_questions() {
        bus.publish(crate::stream::Frame::Questions { waiting });
    }
}

/// Where the caller's questions stand, waiting up to the caller's
/// `wait_secs` for one of them to be answered or cancelled. A question
/// outlives every call that waits on it: the row is the state, so a caller
/// that gives up or is dropped loses nothing, and the next call reads an
/// answer given in between.
async fn question_status(
    store: &Arc<Store>,
    manager: &crate::session::Manager,
    ctx: &CallContext,
    ids: &[String],
    args: &Value,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs(ctx, args));
    loop {
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            let row = store
                .operator_question(id)
                .map_err(|e| e.to_string())?
                .filter(|row| {
                    caller_owns(store, ctx, &row.channel, row.session_id.as_deref())
                        && row.node_id == ctx.node_id
                })
                .ok_or_else(|| format!("no question {id} of yours on this channel"))?;
            if !matches!(row.state.as_str(), "unanswered" | "answered" | "cancelled") {
                return Err("operator question has an invalid state".into());
            }
            rows.push(row);
        }
        let decided = rows.iter().any(|row| row.state != "unanswered");
        if decided || tokio::time::Instant::now() >= deadline {
            // What this call returns decided, the session has been told: the
            // node does not bring it back as a prompt as well.
            if let Some(session_id) = ctx.session_id() {
                for row in rows.iter().filter(|row| row.state != "unanswered") {
                    manager
                        .waits()
                        .tell(session_id, &crate::session::waits::question_key(row));
                }
            }
            let in_session = ctx.session_id().is_some();
            let views: Vec<Value> = rows
                .iter()
                .map(|row| question_view(row, in_session))
                .collect();
            return Ok(if args.get("question_ids").is_some() {
                json!({ "questions": views, "still_waiting": !decided })
            } else {
                let mut view = views.into_iter().next().unwrap_or(Value::Null);
                view["still_waiting"] = json!(!decided);
                view
            });
        }
        sleep(Duration::from_millis(250)).await;
    }
}

/// What `question_status` says of one question. `in_session` when the caller
/// is a session the node runs, which the node holds open while the question
/// waits and brings the answer back to.
pub(crate) fn question_view(row: &OperatorQuestionRow, in_session: bool) -> Value {
    match row.state.as_str() {
        "answered" => json!({
            "question_id": row.id,
            "state": "answered",
            "answer": answer_view(row),
        }),
        "cancelled" => json!({
            "question_id": row.id,
            "state": "cancelled",
            "message": "The operator cancelled this question; no answer was given.",
        }),
        state => json!({
            "question_id": row.id,
            "request_id": row.request_key,
            "state": state,
            "message": format!(
                "The operator has not answered yet; the question stays in their queue. Call \
                 {QUESTION_STATUS} with this question_id to keep waiting; each call waits up \
                 to {MAX_WAIT_SECS} seconds.{}",
                crate::session::waits::brought_back(in_session)
            ),
        }),
    }
}

/// A question's answer as `{choice, text}`. Answers stored before an answer
/// could carry both kept a picked choice as `{text}`; that reads as the
/// choice it was.
fn answer_view(row: &OperatorQuestionRow) -> Value {
    let Some(mut answer) = row
        .answer_json
        .as_deref()
        .and_then(|v| serde_json::from_str::<Value>(v).ok())
    else {
        return Value::Null;
    };
    if let Some(fields) = answer.as_object_mut() {
        let choices: Vec<String> = serde_json::from_str(&row.choices_json).unwrap_or_default();
        let legacy_choice = !fields.contains_key("choice")
            && fields
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| choices.iter().any(|c| c == text));
        if legacy_choice {
            let text = fields.remove("text").unwrap();
            fields.insert("choice".into(), text);
        }
    }
    answer
}

/// The nodes a notification goes to when the caller names none: every node
/// of the channel, as a review's push reaches a phone subscribed at any of
/// them, since each node delivers only to the devices that subscribed there.
/// This node alone when there is no mesh to reach the others through.
fn default_nodes(
    store: &Arc<Store>,
    channel: &str,
    local: &str,
    mesh: bool,
) -> Result<Vec<String>, String> {
    let mut nodes = vec![local.to_string()];
    if mesh {
        nodes.extend(store.nodes_in_channel(channel).map_err(|e| e.to_string())?);
    }
    nodes.sort();
    nodes.dedup();
    Ok(nodes)
}

async fn notify_operator(
    store: &Arc<Store>,
    manager: &crate::session::Manager,
    cfg: &Config,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let title = text(args, "title", true)?;
    let message = text(args, "message", true)?;
    let default_path = match ctx.session_id() {
        Some(id) => format!("/sessions/{id}"),
        None => "/".to_string(),
    };
    let path = match args.get("path") {
        None => default_path.as_str(),
        Some(Value::String(path)) => path.trim(),
        _ => return Err("path must be a string".into()),
    };
    if !valid_local_path(path) {
        return Err("path must be an unencoded local absolute path".into());
    }
    let devices = string_array(args, "device_ids", 32, 256)?;
    let mut nodes = string_array(args, "node_ids", 32, 256)?;
    if nodes.is_empty() {
        nodes = default_nodes(store, &ctx.channel, &ctx.node_id, manager.mesh().is_some())?;
    }
    nodes.sort();
    nodes.dedup();
    let channel = store.channel_get(&ctx.channel).map_err(|e| e.to_string())?;
    if channel
        .as_ref()
        .and_then(|c| serde_json::from_str::<Value>(&c.bindings_json).ok())
        .is_some_and(|b| !notify::enabled(&b))
    {
        return Err("notifications are disabled for this channel".into());
    }
    let mut canonical_devices = devices.clone();
    canonical_devices.sort();
    canonical_devices.dedup();
    let canonical = json!({
        "channel": ctx.channel.clone(), "title": title.clone(), "message": message.clone(), "path": path,
        "device_ids": canonical_devices, "node_ids": nodes.clone(),
    });
    let dedup = hex::encode(Sha256::digest(serde_json::to_vec(&canonical).unwrap()));
    let id = uuid::Uuid::now_v7().to_string();
    if !store
        .claim_operator_notification(&dedup, &id, 60_000)
        .map_err(|e| e.to_string())?
    {
        return Ok(json!({"deduplicated": true, "attempts": []}));
    }
    if !store
        .claim_operator_notification_rate(&ctx.channel, &ctx.node_id, 10, 60_000)
        .map_err(|e| e.to_string())?
    {
        store
            .release_operator_notification(&id)
            .map_err(|e| e.to_string())?;
        return Err("operator notification rate limit exceeded; try again later".into());
    }
    let mesh = manager.mesh();
    let sends = nodes.iter().map(|node| {
        let (title, message, devices, id) = (&title, &message, &devices, &id);
        async move {
            if node == &ctx.node_id {
                let attempts = notify::send_operator(
                    store,
                    cfg,
                    id,
                    title.clone(),
                    message.clone(),
                    path.to_string(),
                    devices,
                )
                .await;
                // An attempt list that is empty reads as nothing having gone
                // wrong; this node had no device to try, and says so.
                if attempts.is_empty() {
                    let _ = store.record_notification_attempt(
                        id,
                        &format!("node:{node}"),
                        "no matching live device",
                    );
                }
                return;
            }
            let Some(mesh) = mesh else {
                let _ = store.record_notification_attempt(
                    id,
                    &format!("node:{node}"),
                    "refused: mesh is disabled",
                );
                return;
            };
            let command = proto::frame::Command::OperatorNotify {
                channel: ctx.channel.clone(),
                notification_id: id.clone(),
                title: title.clone(),
                body: message.clone(),
                path: path.to_string(),
                device_ids: devices.clone(),
            };
            match mesh
                .command(
                    node,
                    command,
                    std::time::Duration::from_secs(cfg.mesh.command_timeout_secs.max(1)),
                )
                .await
            {
                Ok(reply) => {
                    let _ = store.record_notification_attempt(
                        id,
                        &format!("node:{node}"),
                        "peer acknowledged delivery request",
                    );
                    let remote = reply["attempts"].as_array().cloned().unwrap_or_default();
                    if remote.is_empty() {
                        let _ = store.record_notification_attempt(
                            id,
                            &format!("node:{node}"),
                            "peer reports no matching live device",
                        );
                    }
                    for attempt in remote {
                        let device = attempt["device_id"].as_str().unwrap_or("unknown-device");
                        let outcome = attempt["outcome"].as_str().unwrap_or("unknown");
                        let _ = store.record_notification_attempt(
                            id,
                            &format!("node:{node}/{device}"),
                            outcome,
                        );
                    }
                }
                Err(error) => {
                    let _ = store.record_notification_attempt(
                        id,
                        &format!("node:{node}"),
                        &format!("refused: {error}"),
                    );
                }
            }
        }
    });
    // Together, so a member that is offline costs one timeout, not one each.
    futures_util::future::join_all(sends).await;
    Ok(
        json!({"notification_id": id, "deduplicated": false, "attempts": store.notification_attempts(&id).map_err(|e| e.to_string())?, "receipt": "device push-service attempts and peer acknowledgements only; human receipt is unknown"}),
    )
}

fn report(store: &Arc<Store>, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let title =
        report_text(args, "title", GITHUB_ISSUE_TITLE_MAX_CHARS)?.ok_or("title is required")?;
    let title_chars = title.text.chars().count();
    if title_chars > GITHUB_ISSUE_TITLE_MAX_CHARS {
        return Err(format!(
            "title is {} characters once secret-looking lines are redacted ({} before); \
             GitHub allows {}",
            thousands(title_chars),
            thousands(title.input_chars),
            thousands(GITHUB_ISSUE_TITLE_MAX_CHARS),
        ));
    }
    let mut fields = Vec::with_capacity(REPORT_SECTIONS.len());
    for (index, (key, heading)) in REPORT_SECTIONS.into_iter().enumerate() {
        match report_text(args, key, REPORT_FIELD_MAX_CHARS)? {
            Some(field) => fields.push((key, heading, field)),
            None if index < 2 => return Err(format!("{key} is required")),
            None => {}
        }
    }
    let mut body = String::new();
    for (_, heading, field) in &fields {
        body.push_str("## ");
        body.push_str(heading);
        body.push_str("\n\n");
        body.push_str(&field.text);
        body.push_str("\n\n");
    }
    let attachments = Value::Array(attachments(args)?);
    let attachments_json = serde_json::to_string(&attachments).unwrap();
    // The id is in the published marker, so it is chosen before the body is
    // measured: the check below measures exactly the string `gh` is given.
    let id = uuid::Uuid::now_v7().to_string();
    let published = published_issue_body(&body, &attachments, &id);
    let total = published.chars().count();
    if total > REPORT_BODY_MAX_CHARS {
        return Err(over_budget(total, &fields, &attachments));
    }
    store
        .insert_issue_draft(&IssueDraftRow {
            id: id.clone(),
            session_id: ctx.session_id().map(str::to_string),
            channel: ctx.channel.clone(),
            title: title.text,
            body,
            attachments_json,
            state: "draft".into(),
            published_url: None,
            publish_error: None,
            created_ms: now_ms(),
            approved_ms: None,
            published_number: None,
            decided_ms: None,
            discard_reason: None,
            node_id: Some(ctx.node_id.clone()),
        })
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "issue_id": id,
        "state": "draft",
        "next": format!(
            "The operator can inspect and explicitly authorize this draft. Reporting does not \
             pause execution. Call {REPORT_STATUS} with this issue_id to learn whether it was \
             published, and as which GitHub issue, or discarded."
        ),
    }))
}

/// The issue body exactly as `gh issue create` receives it: the drafted
/// sections, the inspectable attachments as pretty-printed JSON, and the
/// marker the operator searches for to reconcile an uncertain publication.
/// Drafting measures this string and publishing sends it, so the two cannot
/// disagree about its length.
pub fn published_issue_body(body: &str, attachments: &Value, id: &str) -> String {
    format!(
        "{body}\n\n## Inspectable attachments\n\n```json\n{}\n```\n\n<!-- tracon-issue-draft:{id} -->",
        published_attachments(attachments),
    )
}

fn published_attachments(attachments: &Value) -> String {
    serde_json::to_string_pretty(attachments).unwrap_or_else(|_| "[]".into())
}

/// Why a report does not fit: the total and the limit, then where the room
/// went, largest field first, so the agent knows what to shorten.
fn over_budget(total: usize, fields: &[(&str, &str, ReportText)], attachments: &Value) -> String {
    let mut sizes: Vec<(&str, usize, usize)> = fields
        .iter()
        .map(|(key, _, field)| (*key, field.text.chars().count(), field.input_chars))
        .collect();
    sizes.sort_by_key(|a| std::cmp::Reverse(a.1));
    let largest: Vec<String> = sizes
        .iter()
        .take(3)
        .map(|(key, chars, input)| {
            if chars > input {
                format!(
                    "{key} {} (redaction added {})",
                    thousands(*chars),
                    thousands(chars - input)
                )
            } else {
                format!("{key} {}", thousands(*chars))
            }
        })
        .collect();
    let in_fields: usize = sizes.iter().map(|(_, chars, _)| chars).sum();
    let in_attachments = published_attachments(attachments).chars().count();
    format!(
        "the published issue body would be {} characters; the limit is {}. Largest fields: {}. \
         Attachments take {} characters once JSON-encoded; headings and the draft marker take {}.",
        thousands(total),
        thousands(REPORT_BODY_MAX_CHARS),
        largest.join(", "),
        thousands(in_attachments),
        thousands(total.saturating_sub(in_fields + in_attachments)),
    )
}

/// `n` with a comma between each group of three digits, as limits are
/// written to the agent.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A `report_issue` text field, trimmed and scrubbed, with the size it was
/// given at so a refusal can say how much redaction added.
struct ReportText {
    text: String,
    input_chars: usize,
}

/// A `report_issue` string, refused when its input is longer than `max`
/// characters (what the schema's `maxLength` says). `None` when absent or
/// blank.
fn report_text(args: &Value, key: &str, max: usize) -> Result<Option<ReportText>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let chars = s.chars().count();
            if chars > max {
                return Err(if key == "title" {
                    format!(
                        "title is {} characters; GitHub allows {}",
                        thousands(chars),
                        thousands(max)
                    )
                } else {
                    format!(
                        "{key} is {} characters; a field may be at most {}",
                        thousands(chars),
                        thousands(max)
                    )
                });
            }
            let v = s.trim();
            Ok((!v.is_empty()).then(|| ReportText {
                text: scrub(v),
                input_chars: v.chars().count(),
            }))
        }
        _ => Err(format!("{key} must be a string")),
    }
}

async fn report_status(
    store: &Arc<Store>,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let ids = ids(args, "issue_id", "issue_ids")?;
    let wait = wait_secs(ctx, args);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let mut rows = Vec::with_capacity(ids.len());
        for id in &ids {
            let row = store
                .issue_draft(id)
                .map_err(|e| e.to_string())?
                .filter(|row| {
                    caller_owns(store, ctx, &row.channel, row.session_id.as_deref())
                        && row.node_id.as_deref().is_none_or(|n| n == ctx.node_id)
                })
                .ok_or_else(|| format!("no issue draft {id} of yours on this channel"))?;
            rows.push(row);
        }
        let decided = rows
            .iter()
            .any(|row| !matches!(row.state.as_str(), "draft" | "publishing"));
        if decided || tokio::time::Instant::now() >= deadline {
            let views: Vec<Value> = rows.iter().map(issue_view).collect();
            return Ok(if args.get("issue_ids").is_some() {
                json!({ "issues": views, "still_waiting": !decided })
            } else {
                let mut view = views.into_iter().next().unwrap_or(Value::Null);
                view["still_waiting"] = json!(!decided);
                view
            });
        }
        sleep(Duration::from_millis(250)).await;
    }
}

fn ids(args: &Value, one: &str, many: &str) -> Result<Vec<String>, String> {
    if let Some(list) = args.get(many) {
        let ids: Vec<String> = list
            .as_array()
            .ok_or_else(|| format!("{many} is a list of ids"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Err(format!("{many} names nothing"));
        }
        return Ok(ids);
    }
    args.get(one)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .ok_or_else(|| format!("{one} or {many} is required"))
}

fn issue_view(row: &IssueDraftRow) -> Value {
    match row.state.as_str() {
        "published" => json!({
            "issue_id": row.id,
            "state": "published",
            "number": row.published_number,
            "url": row.published_url,
            "title": row.title,
        }),
        "discarded" => json!({
            "issue_id": row.id,
            "state": "discarded",
            "reason": row.discard_reason,
        }),
        state => json!({
            "issue_id": row.id,
            "state": state,
            "error": row.publish_error,
        }),
    }
}

fn text(args: &Value, key: &str, required: bool) -> Result<String, String> {
    optional_text(args, key)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            if required {
                format!("{key} is required")
            } else {
                String::new()
            }
        })
}
fn optional_text(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let v = s.trim();
            if v.len() > MAX_TEXT {
                return Err(format!("{key} exceeds {MAX_TEXT} bytes"));
            }
            Ok((!v.is_empty()).then(|| scrub(v)))
        }
        _ => Err(format!("{key} must be a string")),
    }
}

fn valid_local_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.contains('%')
        && !path.split('/').any(|segment| segment == "..")
}
fn attachments(args: &Value) -> Result<Vec<Value>, String> {
    let Some(value) = args.get("attachments") else {
        return Ok(vec![]);
    };
    let items = value.as_array().ok_or("attachments must be an array")?;
    if items.len() > MAX_ATTACHMENTS {
        return Err(format!("at most {MAX_ATTACHMENTS} inspectable attachments"));
    }
    items
        .iter()
        .map(|attachment| {
            let name = attachment
                .get("name")
                .and_then(Value::as_str)
                .ok_or("attachment name is required")?;
            let name_chars = name.chars().count();
            if name_chars > REPORT_ATTACHMENT_NAME_MAX_CHARS {
                return Err(format!(
                    "attachment name is {} characters; an attachment name may be at most {}",
                    thousands(name_chars),
                    thousands(REPORT_ATTACHMENT_NAME_MAX_CHARS),
                ));
            }
            let name = name.trim();
            if name.is_empty() {
                return Err("attachment name is required".into());
            }
            let content = attachment
                .get("content")
                .and_then(Value::as_str)
                .ok_or("attachment content is required")?;
            let content_chars = content.chars().count();
            if content_chars > REPORT_ATTACHMENT_MAX_CHARS {
                return Err(format!(
                    "attachment {name} is {} characters; an attachment may be at most {}",
                    thousands(content_chars),
                    thousands(REPORT_ATTACHMENT_MAX_CHARS),
                ));
            }
            Ok(json!({"name": scrub(name), "content": scrub(content)}))
        })
        .collect()
}

fn string_array(
    args: &Value,
    key: &str,
    max_items: usize,
    max_bytes: usize,
) -> Result<Vec<String>, String> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| format!("{key} must be an array"))?;
    if values.len() > max_items {
        return Err(format!("at most {max_items} {key}"));
    }
    values
        .iter()
        .map(|value| {
            let value = value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{key} must contain nonempty strings"))?;
            if value.len() > max_bytes {
                return Err(format!("{key} entries exceed {max_bytes} bytes"));
            }
            Ok(value.to_string())
        })
        .collect()
}
/// `s` with every line that looks like a credential replaced.
pub(crate) fn scrub(s: &str) -> String {
    let mut in_pem = false;
    let mut out = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN ") {
            in_pem = true;
        }
        let lowered = trimmed.to_ascii_lowercase();
        let credential_assignment = [
            "password=",
            "password:",
            "token=",
            "token:",
            "secret=",
            "secret:",
            "api_key=",
            "api_key:",
            "authorization:",
            "cookie:",
        ]
        .iter()
        .any(|marker| lowered.contains(marker));
        if in_pem
            || credential_assignment
            || lowered.starts_with("bearer ")
            || lowered.contains("ghp_")
            || lowered.contains("github_pat_")
            || contains_secret_token(&lowered)
        {
            out.push("[redacted secret-looking content]".to_string());
        } else {
            out.push(line.to_string());
        }
        if trimmed.starts_with("-----END ") {
            in_pem = false;
        }
    }

    out.join("\n")
}

fn contains_secret_token(line: &str) -> bool {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .any(|token| {
            token.starts_with("sk-")
                && token.len() >= 20
                && token[3..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limits are pinned: the published body keeps its stated margin
    /// under GitHub's, and the title limit is GitHub's.
    #[test]
    fn report_limits_leave_a_margin_under_githubs() {
        assert_eq!(GITHUB_ISSUE_TITLE_MAX_CHARS, 256);
        assert_eq!(GITHUB_ISSUE_BODY_MAX_CHARS, 65_536);
        assert_eq!(REPORT_BODY_MAX_CHARS, 60_000);
        let with_margin = REPORT_BODY_MAX_CHARS + REPORT_BODY_MARGIN_CHARS;
        assert!(with_margin <= GITHUB_ISSUE_BODY_MAX_CHARS, "{with_margin}");
        assert_eq!(REPORT_FIELD_MAX_CHARS, REPORT_BODY_MAX_CHARS);
        assert_eq!(REPORT_ATTACHMENT_MAX_CHARS, 8_192);
        assert_eq!(MAX_ATTACHMENTS, 3);
    }

    #[test]
    fn limits_are_written_with_thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(256), "256");
        assert_eq!(thousands(8_192), "8,192");
        assert_eq!(thousands(60_000), "60,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    /// A phone subscribes at whichever node it reaches, so a notification
    /// that names no node goes to every node of the channel; with no mesh
    /// there is nothing to reach them through, and it stays here.
    #[test]
    fn a_notification_naming_no_node_goes_to_every_node_of_the_channel() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.node_channel_add("laptop", "personal").unwrap();
        store.node_channel_add("homelab", "personal").unwrap();
        store.node_channel_add("other", "work").unwrap();
        assert_eq!(
            default_nodes(&store, "personal", "laptop", true).unwrap(),
            ["homelab", "laptop"]
        );
        assert_eq!(
            default_nodes(&store, "personal", "laptop", false).unwrap(),
            ["laptop"]
        );
    }
}
