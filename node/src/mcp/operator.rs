use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, Duration};

use crate::{
    config::Config,
    mcp::CallContext,
    notify,
    store::{now_ms, IssueDraftRow, OperatorQuestionRow, Store},
};

pub const ASK: &str = "ask_operator";
pub const NOTIFY: &str = "notify_operator";
pub const REPORT: &str = "report_issue";
const MAX_TEXT: usize = 8 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 8 * 1024;
const MAX_ATTACHMENTS: usize = 3;

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"name": ASK, "description":"Ask the operator a free-text question, optionally choosing from explicit choices. Supply a stable request_id to recover an unanswered or answered question after reconnect/restart; it never grants access.", "inputSchema":{"type":"object","properties":{"request_id":{"type":"string","maxLength":128},"question":{"type":"string"},"choices":{"type":"array","items":{"type":"string"},"maxItems":20}},"required":["request_id","question"]}}),
        json!({"name": NOTIFY, "description":"Intentionally send an OS/PWA notification to configured operator devices. A push-service attempt is not evidence a human read it.", "inputSchema":{"type":"object","properties":{"title":{"type":"string"},"message":{"type":"string"},"path":{"type":"string"},"device_ids":{"type":"array","items":{"type":"string"}},"node_ids":{"type":"array","items":{"type":"string"}}},"required":["title","message"]}}),
        json!({"name": REPORT, "description":"Draft a report about tracon, its environments, tools, or harness integration. It never pauses execution or uploads repository files/transcripts. The operator inspects and authorizes publication separately.", "inputSchema":{"type":"object","properties":{"title":{"type":"string"},"expected":{"type":"string"},"actual":{"type":"string"},"reproduction":{"type":"string"},"versions":{"type":"string"},"errors":{"type":"string"},"recovery":{"type":"string"},"diagnosis":{"type":"string"},"attachments":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"},"content":{"type":"string"}},"required":["name","content"]},"maxItems":3}},"required":["title","expected","actual"]}}),
    ]
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
    let external = store
        .get_session(&ctx.session_id)
        .map_err(|e| e.to_string())?
        .is_some_and(|session| session.harness_id == crate::session::external::HARNESS_ID);
    let existing = if external {
        store.operator_question_for_channel_request(&ctx.channel, &request_key)
    } else {
        store.operator_question_for_request(&ctx.session_id, &request_key)
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
                session_id: ctx.session_id.clone(),
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
    wait_for_question(store, row.id).await
}

async fn wait_for_question(store: &Arc<Store>, id: String) -> Result<Value, String> {
    loop {
        let row = store
            .operator_question(&id)
            .map_err(|e| e.to_string())?
            .ok_or("operator question disappeared")?;
        match row.state.as_str() {
            "answered" => {
                return Ok(
                    json!({"question_id": id, "answer": row.answer_json.and_then(|v| serde_json::from_str::<Value>(&v).ok()).unwrap_or(Value::Null)}),
                )
            }
            "cancelled" => {
                return Err("the operator cancelled this question; no answer was given".into())
            }
            "unanswered" => sleep(Duration::from_millis(250)).await,
            _ => return Err("operator question has an invalid state".into()),
        }
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
    let default_path = format!("/sessions/{}", ctx.session_id);
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
            session_id: ctx.session_id.clone(),
            channel: ctx.channel.clone(),
            title,
            body,
            attachments_json,
            state: "draft".into(),
            published_url: None,
            publish_error: None,
            created_ms: now_ms(),
            approved_ms: None,
        })
        .map_err(|e| e.to_string())?;
    Ok(
        json!({"issue_id": id, "state":"draft", "next":"The operator can inspect and explicitly authorize this draft. Reporting does not pause execution."}),
    )
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
