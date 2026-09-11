use std::sync::Arc;

use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::{config::Config, mcp::CallContext, notify, store::{IssueDraftRow, OperatorQuestionRow, Store, now_ms}};

pub const ASK: &str = "ask_operator";
pub const NOTIFY: &str = "notify_operator";
pub const REPORT: &str = "report_issue";
const MAX_TEXT: usize = 8 * 1024;
const MAX_ATTACHMENT_BYTES: usize = 8 * 1024;
const MAX_ATTACHMENTS: usize = 3;

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"name": ASK, "description":"Ask the operator a free-text question, optionally choosing from explicit choices. This is not a permission request and does not grant access.", "inputSchema":{"type":"object","properties":{"question":{"type":"string"},"choices":{"type":"array","items":{"type":"string"},"maxItems":20}},"required":["question"]}}),
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
    let prompt = text(args, "question", true)?;
    let choices: Vec<String> = args.get("choices").and_then(Value::as_array).map(|xs| xs.iter().map(|x| x.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).ok_or("choices must contain nonempty strings".to_string())).collect::<Result<_,_>>()).transpose()?.unwrap_or_default();
    if choices.len() > 20 || choices.iter().any(|choice| choice.len() > MAX_TEXT) {
        return Err("at most 20 choices of up to 8192 bytes".into());
    }
    let id = uuid::Uuid::now_v7().to_string();
    store.insert_operator_question(&OperatorQuestionRow {
        id: id.clone(), session_id: ctx.session_id.clone(), channel: ctx.channel.clone(),
        node_id: ctx.node_id.clone(), prompt, choices_json: serde_json::to_string(&choices).unwrap(),
        state: "unanswered".into(), answer_json: None, created_ms: now_ms(), answered_ms: None,
    }).map_err(|e| e.to_string())?;
    loop {
        let row = store.operator_question(&id).map_err(|e| e.to_string())?.ok_or("operator question disappeared")?;
        match row.state.as_str() {
            "answered" => return Ok(json!({"question_id": id, "answer": row.answer_json.and_then(|v| serde_json::from_str::<Value>(&v).ok()).unwrap_or(Value::Null)})),
            "cancelled" => return Err("the operator cancelled this question; no answer was given".into()),
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
    let path = args.get("path").and_then(Value::as_str).unwrap_or("/").trim();
    if !valid_local_path(path) {
        return Err("path must be an unencoded local absolute path".into());
    }
    let devices = args.get("device_ids").and_then(Value::as_array).map(|xs| xs.iter().map(|v| v.as_str().map(str::to_string).ok_or("device_ids must be strings".to_string())).collect::<Result<Vec<_>,_>>()).transpose()?.unwrap_or_default();
    if devices.len() > 32 || devices.iter().any(|id| id.is_empty() || id.len() > 256) {
        return Err("at most 32 nonempty device ids of up to 256 bytes".into());
    }
    let mut nodes = args.get("node_ids").and_then(Value::as_array).map(|xs| xs.iter().map(|v| v.as_str().map(str::to_string).ok_or("node_ids must be strings".to_string())).collect::<Result<Vec<_>,_>>()).transpose()?.unwrap_or_default();
    if nodes.len() > 32 || nodes.iter().any(|id| id.is_empty() || id.len() > 256) {
        return Err("at most 32 nonempty node ids of up to 256 bytes".into());
    }
    if nodes.is_empty() {
        nodes.push(ctx.node_id.clone());
    }
    nodes.sort();
    nodes.dedup();
    let channel = store.channel_get(&ctx.channel).map_err(|e| e.to_string())?;
    if channel.as_ref().and_then(|c| serde_json::from_str::<Value>(&c.bindings_json).ok()).is_some_and(|b| !notify::enabled(&b)) {
        return Err("notifications are disabled for this channel".into());
    }
    let dedup = format!("{}\n{}\n{}\n{}\n{}\n{}", ctx.channel, title, message, path, devices.join(","), nodes.join(","));
    let id = uuid::Uuid::now_v7().to_string();
    if !store.claim_operator_notification(&dedup, &id, 60_000).map_err(|e| e.to_string())? {
        return Ok(json!({"deduplicated": true, "attempts": []}));
    }
    if !store
        .claim_operator_notification_rate(&ctx.channel, &ctx.node_id, 10, 60_000)
        .map_err(|e| e.to_string())?
    {
        store.release_operator_notification(&id).map_err(|e| e.to_string())?;
        return Err("operator notification rate limit exceeded; try again later".into());
    }
    let mesh = manager.mesh();
    for node in &nodes {
        if node == &ctx.node_id {
            let _ = notify::send_operator(store, cfg, &id, title.clone(), message.clone(), path.to_string(), &devices).await;
            continue;
        }
        let Some(mesh) = mesh else {
            let _ = store.record_notification_attempt(&id, &format!("node:{node}"), "refused: mesh is disabled");
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
        let outcome = match mesh.command(node, command, std::time::Duration::from_secs(cfg.mesh.command_timeout_secs.max(1))).await {
            Ok(_) => "acknowledged".to_string(),
            Err(error) => format!("refused: {error}"),
        };
        let _ = store.record_notification_attempt(&id, &format!("node:{node}"), &outcome);
    }
    Ok(json!({"notification_id": id, "deduplicated": false, "attempts": store.notification_attempts(&id).map_err(|e| e.to_string())?, "receipt": "push-service outcomes only; human receipt is unknown"}))
}

fn report(store: &Arc<Store>, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let title = text(args, "title", true)?;
    let expected = text(args, "expected", true)?;
    let actual = text(args, "actual", true)?;
    let sections = [
        ("Expected behavior", Some(expected)),
        ("Actual behavior", Some(actual)),
        ("Reproduction", optional_text(args, "reproduction")?),
        ("Versions", optional_text(args, "versions")?),
        ("Relevant errors", optional_text(args, "errors")?),
        ("Attempted recovery", optional_text(args, "recovery")?),
        ("Agent diagnosis (not observed evidence)", optional_text(args, "diagnosis")?),
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
    let id = uuid::Uuid::now_v7().to_string();
    store.insert_issue_draft(&IssueDraftRow { id: id.clone(), session_id: ctx.session_id.clone(), channel: ctx.channel.clone(), title, body, attachments_json: serde_json::to_string(&attachments).unwrap(), state: "draft".into(), published_url: None, publish_error: None, created_ms: now_ms(), approved_ms: None }).map_err(|e| e.to_string())?;
    Ok(json!({"issue_id": id, "state":"draft", "next":"The operator can inspect and explicitly authorize this draft. Reporting does not pause execution."}))
}

fn text(args: &Value, key: &str, required: bool) -> Result<String, String> { optional_text(args, key)?.filter(|s| !s.is_empty()).ok_or_else(|| if required { format!("{key} is required") } else { String::new() }) }
fn optional_text(args: &Value, key: &str) -> Result<Option<String>, String> { match args.get(key) { None | Some(Value::Null) => Ok(None), Some(Value::String(s)) => { let v=s.trim(); if v.len()>MAX_TEXT { return Err(format!("{key} exceeds {MAX_TEXT} bytes")); } Ok((!v.is_empty()).then(|| scrub(v))) }, _ => Err(format!("{key} must be a string")) } }

fn valid_local_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.contains('%')
        && !path.split('/').any(|segment| segment == "..")
}
fn attachments(args: &Value) -> Result<Vec<Value>, String> { let Some(items)=args.get("attachments").and_then(Value::as_array) else { return Ok(vec![]); }; if items.len()>MAX_ATTACHMENTS{return Err(format!("at most {MAX_ATTACHMENTS} inspectable attachments"));} items.iter().map(|v| { let name=v.get("name").and_then(Value::as_str).map(str::trim).filter(|s|!s.is_empty()).ok_or("attachment name is required")?; let content=v.get("content").and_then(Value::as_str).ok_or("attachment content is required")?; if content.len()>MAX_ATTACHMENT_BYTES{return Err(format!("attachment {name} exceeds {MAX_ATTACHMENT_BYTES} bytes"));} Ok(json!({"name":name,"content":scrub(content)})) }).collect() }
fn scrub(s: &str) -> String {
    s.lines()
        .map(|line| {
            let lowered = line.to_ascii_lowercase();
            if ["password", "token", "secret", "authorization:", "bearer ", "api_key", "cookie:", "private key"]
                .iter()
                .any(|marker| lowered.contains(marker))
                || lowered.trim_start().starts_with("ghp_")
                || lowered.trim_start().starts_with("sk-")
            {
                "[redacted secret-looking content]".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
