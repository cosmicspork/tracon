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
const MAX_TEXT: usize = 8 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 8 * 1024;
const MAX_ATTACHMENTS: usize = 3;

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": ASK,
            "description": format!(
                "Ask the operator a free-text question, optionally choosing from explicit \
                 choices. Waits up to {MAX_WAIT_SECS} seconds for the answer and returns \
                 `answered` with it, `cancelled` if the operator withdrew the question, or \
                 `unanswered` with `still_waiting: true` and a `question_id` when they have not \
                 answered yet. The question stays in their queue either way, and an answer given \
                 later is kept: call {QUESTION_STATUS} with the question_id to keep waiting. A \
                 stable request_id recovers the same question after a reconnect or restart. An \
                 answer never grants access."
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
                 answered or cancelled, or after up to {MAX_WAIT_SECS} seconds, with every id's \
                 state — `unanswered` while the operator has not answered, `answered` with the \
                 `answer`, or `cancelled` when they withdrew it without answering. \
                 `still_waiting` is true when nothing was decided; call again to keep waiting."
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
        json!({"name": NOTIFY, "description":"Intentionally send an OS/PWA notification to configured operator devices. A push-service attempt is not evidence a human read it.", "inputSchema":{"type":"object","properties":{"title":{"type":"string"},"message":{"type":"string"},"path":{"type":"string"},"device_ids":{"type":"array","items":{"type":"string"}},"node_ids":{"type":"array","items":{"type":"string"}}},"required":["title","message"]}}),
        json!({"name": REPORT, "description":"Draft a report about tracon, its environments, tools, or harness integration. It never pauses execution or uploads repository files/transcripts. The operator inspects and authorizes publication separately.", "inputSchema":{"type":"object","properties":{"title":{"type":"string"},"expected":{"type":"string"},"actual":{"type":"string"},"reproduction":{"type":"string"},"versions":{"type":"string"},"errors":{"type":"string"},"recovery":{"type":"string"},"diagnosis":{"type":"string"},"attachments":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"content":{"type":"string"}},"required":["name","content"]},"maxItems":3}},"required":["title","expected","actual"]}}),
        json!({
            "name": REPORT_STATUS,
            "description": format!(
                "What became of issues you drafted with {REPORT}. Pass `issue_id`, or \
                 `issue_ids` to wait on several: the call returns as soon as any of them is \
                 decided, or after up to {MAX_WAIT_SECS} seconds, with every id's state — \
                 `draft` or `publishing` while the operator has not decided, `published` with \
                 the GitHub issue `number`, `url` and `title`, `discarded` with the operator's \
                 `reason`, or `uncertain` when publication may or may not have reached GitHub. \
                 `still_waiting` is true when nothing was decided; call again to keep waiting."
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

fn wait_secs_schema() -> Value {
    json!({
        "type": "integer",
        "description": format!(
            "How long to block, up to {MAX_WAIT_SECS}; larger values are capped. \
             Defaults to {MAX_WAIT_SECS}. 0 returns the current state."
        ),
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
        ASK => ask(store, ctx, args).await,
        NOTIFY => notify_operator(store, manager, cfg, ctx, args).await,
        REPORT => report(store, ctx, args),
        REPORT_STATUS => report_status(store, ctx, args).await,
        QUESTION_STATUS => {
            let ids = ids(args, "question_id", "question_ids")?;
            question_status(store, ctx, &ids, args).await
        }
        _ => Err(format!("no operator tool named {name}")),
    }
}

async fn ask(store: &Arc<Store>, ctx: &CallContext, args: &Value) -> Result<Value, String> {
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
        None => store.operator_question_for_channel_request(&ctx.channel, &request_key),
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
            stored
        }
    };
    question_status(store, ctx, &[row.id], args).await
}

/// Where the caller's questions stand, waiting up to the caller's
/// `wait_secs` for one of them to be answered or cancelled. A question
/// outlives every call that waits on it: the row is the state, so a caller
/// that gives up or is dropped loses nothing, and the next call reads an
/// answer given in between.
async fn question_status(
    store: &Arc<Store>,
    ctx: &CallContext,
    ids: &[String],
    args: &Value,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs(args));
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
            let views: Vec<Value> = rows.iter().map(question_view).collect();
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

fn question_view(row: &OperatorQuestionRow) -> Value {
    match row.state.as_str() {
        "answered" => json!({
            "question_id": row.id,
            "state": "answered",
            "answer": row
                .answer_json
                .as_deref()
                .and_then(|v| serde_json::from_str::<Value>(v).ok())
                .unwrap_or(Value::Null),
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
                "The operator has not answered yet. The question stays in their queue and an \
                 answer given meanwhile is kept. Call {QUESTION_STATUS} with this question_id to \
                 keep waiting; each call waits up to {MAX_WAIT_SECS} seconds."
            ),
        }),
    }
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
        nodes.push(ctx.node_id.clone());
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
    for node in &nodes {
        if node == &ctx.node_id {
            let _ = notify::send_operator(
                store,
                cfg,
                &id,
                title.clone(),
                message.clone(),
                path.to_string(),
                &devices,
            )
            .await;
            continue;
        }
        let Some(mesh) = mesh else {
            let _ = store.record_notification_attempt(
                &id,
                &format!("node:{node}"),
                "refused: mesh is disabled",
            );
            continue;
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
                    &id,
                    &format!("node:{node}"),
                    "peer acknowledged delivery request",
                );
                let remote = reply["attempts"].as_array().cloned().unwrap_or_default();
                if remote.is_empty() {
                    let _ = store.record_notification_attempt(
                        &id,
                        &format!("node:{node}"),
                        "peer reports no matching live device",
                    );
                }
                for attempt in remote {
                    let device = attempt["device_id"].as_str().unwrap_or("unknown-device");
                    let outcome = attempt["outcome"].as_str().unwrap_or("unknown");
                    let _ = store.record_notification_attempt(
                        &id,
                        &format!("node:{node}/{device}"),
                        outcome,
                    );
                }
            }
            Err(error) => {
                let _ = store.record_notification_attempt(
                    &id,
                    &format!("node:{node}"),
                    &format!("refused: {error}"),
                );
            }
        }
    }
    Ok(
        json!({"notification_id": id, "deduplicated": false, "attempts": store.notification_attempts(&id).map_err(|e| e.to_string())?, "receipt": "device push-service attempts and peer acknowledgements only; human receipt is unknown"}),
    )
}

fn report(store: &Arc<Store>, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let title = text(args, "title", true)?;
    if title.len() > 256 {
        return Err("title exceeds GitHub's 256-byte limit".into());
    }
    let expected = text(args, "expected", true)?;
    let actual = text(args, "actual", true)?;
    let sections = [
        ("Expected behavior", Some(expected)),
        ("Actual behavior", Some(actual)),
        ("Reproduction", optional_text(args, "reproduction")?),
        ("Versions", optional_text(args, "versions")?),
        ("Relevant errors", optional_text(args, "errors")?),
        ("Attempted recovery", optional_text(args, "recovery")?),
        (
            "Agent diagnosis (not observed evidence)",
            optional_text(args, "diagnosis")?,
        ),
    ];
    let mut body = String::new();
    for (heading, value) in sections {
        if let Some(value) = value {
            body.push_str("## ");

            body.push_str(heading);
            body.push_str("\n\n");
            body.push_str(&value);
            body.push_str("\n\n");
        }
    }
    let attachments = attachments(args)?;
    let attachments_json = serde_json::to_string(&attachments).unwrap();
    if body.len() + attachments_json.len() + 128 > 60_000 {
        return Err("report exceeds the bounded GitHub issue payload".into());
    }
    let id = uuid::Uuid::now_v7().to_string();
    store
        .insert_issue_draft(&IssueDraftRow {
            id: id.clone(),
            session_id: ctx.session_id().map(str::to_string),
            channel: ctx.channel.clone(),
            title,
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

async fn report_status(
    store: &Arc<Store>,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let ids = ids(args, "issue_id", "issue_ids")?;
    let wait = wait_secs(args);
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
                .map(str::trim)
                .filter(|name| !name.is_empty() && name.len() <= 256)
                .ok_or("attachment name must be 1–256 bytes")?;
            let content = attachment
                .get("content")
                .and_then(Value::as_str)
                .ok_or("attachment content is required")?;
            if content.len() > MAX_ATTACHMENT_BYTES {
                return Err(format!(
                    "attachment {name} exceeds {MAX_ATTACHMENT_BYTES} bytes"
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
fn scrub(s: &str) -> String {
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
