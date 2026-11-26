//! What the node does when a plugin calls it back, on the socket it serves
//! each instance (`crate::plugin::host`): the plugin raises a card for the
//! operator and waits on it, pushes a notification, or calls another
//! plugin's tool, always as itself.
//!
//! - `approval/request` puts a card in the operator's queue with the content
//!   the plugin gives, in formats the node renders: `markdown`, `diff` (a
//!   unified diff), `word_diff` (a text before and after, which the page
//!   diffs by word), `table` (key and value rows) and `source`. The card
//!   names the plugin as who asked. A plugin raising it while it serves an
//!   agent's call passes that call's signed caller context as `on_behalf`;
//!   once the node has checked it is the node's own, for this instance and
//!   channel, and recent, the card is that agent's session's too, so the
//!   session waits on it and hears the decision.
//! - `approval/status` waits for a decision on cards this plugin raised on
//!   this channel, at most 45 seconds a call, as an agent's status call
//!   does. Allowing a card runs nothing on the node: the plugin does what
//!   it asked to once it reads `approved`.
//! - `approval/withdraw` takes back a card this plugin raised on this
//!   channel while it still waits, once it is moot (the person decided the
//!   same thing in the plugin's own page): it leaves the queue as
//!   `withdrawn`, with the plugin's reason and the plugin as who did it,
//!   never the operator. A card no longer waiting is left as it is and its
//!   state returned, so a retry is safe.
//! - `notify` pushes to the operator's devices as `notify_operator` does,
//!   its title led by the plugin's name.
//! - `tools/call` calls another plugin's tool as the plugin itself (caller
//!   kind `plugin`), decided by the signed policy and the callee's
//!   `[[plugin]]` entry and logged on the channel: never held, so one that
//!   would be asked is refused.

use std::time::Duration;

use serde_json::{json, Map, Value};

use super::{wait::MAX_WAIT_SECS, CallContext, OperatorCallError, SessionAccess, Tools};
use crate::plugin::host::{self, Error, From, FAILED, INVALID_PARAMS, REFUSED};
use crate::store::approvals::{
    ApprovalRow, CHANGES_REQUESTED, EXPIRED, PENDING, PLUGIN_CARD_TOOL, REJECTED, RUNNING,
    SUCCEEDED, WITHDRAWN,
};

/// The formats a card's content is written in, each rendered by the node.
pub const FORMATS: &[&str] = &["markdown", "diff", "word_diff", "table", "source"];

/// The longest card title, in characters, before the plugin's name.
const MAX_TITLE: usize = 200;
/// The longest summary, in characters.
const MAX_SUMMARY: usize = 16_000;
/// The most blocks one card holds.
const MAX_BLOCKS: usize = 64;
/// The most bytes a card's content holds, as JSON.
const MAX_CONTENT: usize = 512 * 1024;
/// The longest `request_key`.
const MAX_REQUEST_KEY: usize = 128;
/// The shortest a plugin may ask its card to wait, in seconds.
const MIN_EXPIRY_SECS: u64 = 60;
/// The longest reason a plugin gives for withdrawing a card, in characters.
const MAX_WITHDRAW_REASON: usize = 1_000;
/// How old a caller context may be to say whom a card is for: a card is
/// raised while the call it came from is served, not long after.
const ON_BEHALF_WITHIN_MS: i64 = 10 * 60_000;

#[async_trait::async_trait]
impl host::Host for Tools {
    async fn call(&self, from: &From, method: &str, params: Value) -> Result<Value, Error> {
        let access = self
            .session
            .get()
            .ok_or_else(|| Error::failed("this node holds no sessions, so no plugins"))?;
        match method {
            "approval/request" => request(self, access, from, &params).await,
            "approval/status" => status(access, from, &params).await,
            "approval/withdraw" => withdraw(access, from, &params).await,
            "notify" => notify(self, access, from, &params).await,
            "tools/call" => call_tool(self, access, from, &params).await,
            other => Err(Error::new(
                host::METHOD_NOT_FOUND,
                format!("the node has no method {other}"),
            )),
        }
    }
}

fn object<'a>(params: &'a Value, what: &str) -> Result<&'a Map<String, Value>, Error> {
    params
        .as_object()
        .ok_or_else(|| Error::params(format!("{what} takes an object of params")))
}

/// Refuse a key `map` holds that `known` does not name.
fn only(map: &Map<String, Value>, known: &[&str], what: &str) -> Result<(), Error> {
    match map.keys().find(|k| !known.contains(&k.as_str())) {
        Some(key) => Err(Error::params(format!(
            "{what}: unknown key {key:?} (it takes {})",
            known.join(", ")
        ))),
        None => Ok(()),
    }
}

fn string<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    what: &str,
    required: bool,
) -> Result<Option<&'a str>, Error> {
    match map.get(key) {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        None | Some(Value::Null) => Err(Error::params(format!("{what}: {key} is required"))),
        Some(_) => Err(Error::params(format!("{what}: {key} must be a string"))),
    }
}

/// One block of a card's content, checked and written back with only the
/// keys its format takes.
fn block(index: usize, value: &Value) -> Result<Value, Error> {
    let what = format!("content[{index}]");
    let map = object(value, &what)?;
    let format = string(map, "format", &what, true)?.unwrap_or_default();
    let title = string(map, "title", &what, false)?;
    let mut out = json!({ "format": format });
    if let Some(title) = title {
        out["title"] = json!(title);
    }
    let text = |key: &str| -> Result<Value, Error> {
        Ok(json!(string(map, key, &what, true)?.unwrap_or_default()))
    };
    match format {
        "markdown" | "diff" => {
            only(map, &["format", "title", "text"], &what)?;
            out["text"] = text("text")?;
        }
        "source" => {
            only(map, &["format", "title", "text", "language"], &what)?;
            out["text"] = text("text")?;
            if let Some(language) = string(map, "language", &what, false)? {
                out["language"] = json!(language);
            }
        }
        "word_diff" => {
            only(map, &["format", "title", "before", "after"], &what)?;
            out["before"] = text("before")?;
            out["after"] = text("after")?;
        }
        "table" => {
            only(map, &["format", "title", "rows"], &what)?;
            let rows = map.get("rows").and_then(Value::as_array).ok_or_else(|| {
                Error::params(format!("{what}: rows is a list of {{key, value}}"))
            })?;
            let mut shown = Vec::with_capacity(rows.len());
            for (i, row) in rows.iter().enumerate() {
                let at = format!("{what}.rows[{i}]");
                let row = object(row, &at)?;
                only(row, &["key", "value"], &at)?;
                shown.push(json!({
                    "key": string(row, "key", &at, true)?,
                    "value": string(row, "value", &at, true)?,
                }));
            }
            out["rows"] = Value::Array(shown);
        }
        other => {
            return Err(Error::params(format!(
                "{what}: format {other:?} is not one the node renders ({})",
                FORMATS.join(", ")
            )))
        }
    }
    Ok(out)
}

/// A card's content: one block or more, each in a format the node renders.
pub fn content(value: Option<&Value>) -> Result<Vec<Value>, Error> {
    let blocks = value
        .and_then(Value::as_array)
        .ok_or_else(|| Error::params("content is a list of blocks"))?;
    if blocks.is_empty() || blocks.len() > MAX_BLOCKS {
        return Err(Error::params(format!(
            "content holds 1 to {MAX_BLOCKS} blocks, not {}",
            blocks.len()
        )));
    }
    let out: Vec<Value> = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| block(i, b))
        .collect::<Result<_, _>>()?;
    let size = Value::Array(out.clone()).to_string().len();
    if size > MAX_CONTENT {
        return Err(Error::params(format!(
            "content is {size} bytes; a card holds at most {MAX_CONTENT}"
        )));
    }
    Ok(out)
}

/// Whom a card is for, from the caller context of the call the plugin is
/// serving: the session it names, if it is one on this channel, and the
/// caller as the card shows it. Only a context the node signed for this
/// instance and channel, issued in the last few minutes, says anything.
fn on_behalf(
    access: &SessionAccess,
    from: &From,
    context: &Value,
) -> Result<(Option<String>, Value), Error> {
    use crate::plugin::context;
    let refuse = |why: &str| {
        Err(Error::new(
            REFUSED,
            format!("on_behalf is not a caller context the node gave this instance: {why}"),
        ))
    };
    let key = access
        .manager
        .plugins()
        .verifying_key()
        .ok_or_else(|| Error::failed("this node cannot check caller contexts"))?;
    if !context.is_object() || !context::signed_by(context, &key) {
        return refuse("it is not signed by this node");
    }
    if context["plugin"] != from.plugin.as_str()
        || context["instance"] != context::instance_of(&from.token).as_str()
    {
        return refuse("it was sent to another plugin or an earlier start");
    }
    if context["channel"] != from.channel.as_str() {
        return refuse("it is for another channel");
    }
    let issued = context["issued_ms"].as_i64().unwrap_or(0);
    if crate::store::now_ms() - issued > ON_BEHALF_WITHIN_MS {
        return refuse("it is too old to say whom a card is for");
    }
    let caller = &context["caller"];
    if caller["kind"] != "agent" {
        return refuse("only an agent's call is one a card is raised on behalf of");
    }
    let session = caller["session"].as_str().map(str::to_string);
    if let Some(id) = &session {
        let row = access
            .store
            .get_session(id)
            .map_err(|e| Error::failed(e.to_string()))?;
        if row.is_none_or(|s| s.channel != from.channel) {
            return refuse("its session is not one on this channel");
        }
    }
    Ok((
        session,
        json!({
            "kind": "agent",
            "session": caller["session"],
            "harness": caller["harness"],
        }),
    ))
}

async fn request(
    tools: &Tools,
    access: &SessionAccess,
    from: &From,
    params: &Value,
) -> Result<Value, Error> {
    const WHAT: &str = "approval/request";
    let map = object(params, WHAT)?;
    only(
        map,
        &[
            "title",
            "summary",
            "content",
            "request_key",
            "expires_secs",
            "on_behalf",
        ],
        WHAT,
    )?;
    let title = string(map, "title", WHAT, true)?.unwrap_or_default().trim();
    let title_chars = title.chars().count();
    if title_chars == 0 || title_chars > MAX_TITLE || title.contains('\n') {
        return Err(Error::params(format!(
            "title is one line of 1 to {MAX_TITLE} characters"
        )));
    }
    let summary = string(map, "summary", WHAT, false)?;
    if summary.is_some_and(|s| s.chars().count() > MAX_SUMMARY) {
        return Err(Error::params(format!(
            "summary holds at most {MAX_SUMMARY} characters"
        )));
    }
    let blocks = content(map.get("content"))?;
    let request_key = string(map, "request_key", WHAT, false)?;
    if request_key.is_some_and(|k| {
        k.is_empty()
            || k.len() > MAX_REQUEST_KEY
            || !k
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    }) {
        return Err(Error::params(format!(
            "request_key is up to {MAX_REQUEST_KEY} letters, digits, '.', '-' or '_'"
        )));
    }
    let longest = tools.cfg.live().session.approval_expiry_secs.max(1);
    let expires_secs = match map.get("expires_secs") {
        None | Some(Value::Null) => longest,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| Error::params("expires_secs is a whole number of seconds"))?
            .clamp(MIN_EXPIRY_SECS.min(longest), longest),
    };
    let (session_id, requested_for) = match map.get("on_behalf") {
        None | Some(Value::Null) => (None, Value::Null),
        Some(context) => on_behalf(access, from, context)?,
    };
    let arguments = json!({
        "plugin": from.plugin,
        "version": from.version,
        "title": title,
        "summary": summary,
        "content": blocks,
        "requested_for": requested_for,
    });
    // The same request while its card waits is that card: by the key the
    // plugin gives, or else by everything it asked.
    let asked = match request_key {
        Some(key) => key.to_string(),
        None => crate::corpus::hash_body(&arguments.to_string()),
    };
    let request_key = crate::corpus::hash_body(&format!(
        "{PLUGIN_CARD_TOOL}\u{1f}{}\u{1f}{asked}",
        from.plugin
    ));
    let store = &access.store;
    if let Some(card) = store
        .pending_approval(&from.channel, &request_key)
        .map_err(|e| Error::failed(e.to_string()))?
    {
        return Ok(json!({ "approval_id": card.id, "state": "pending", "new": false }));
    }
    let now = crate::store::now_ms();
    let lane = CallContext::plugin(&from.plugin, &from.channel, "")
        .lane()
        .map(str::to_string);
    let row = ApprovalRow {
        id: uuid::Uuid::now_v7().to_string(),
        channel: from.channel.clone(),
        session_id,
        node_id: access.manager.node_id().to_string(),
        lane,
        tool: PLUGIN_CARD_TOOL.to_string(),
        arguments: arguments.to_string(),
        request_key,
        title: format!("{}: {title}", from.plugin),
        state: PENDING.into(),
        answer_option_id: None,
        edited_arguments: None,
        result: None,
        reason: None,
        operator_note: None,
        claimed_ms: None,
        created_ms: now,
        decided_ms: None,
        finished_ms: None,
        expires_ms: now + (expires_secs as i64) * 1000,
    };
    store
        .insert_approval(&row)
        .map_err(|e| Error::failed(e.to_string()))?;
    access.manager.approval_requested(&row).await;
    Ok(json!({ "approval_id": row.id, "state": "pending", "new": true }))
}

/// Whether `row` is a card `from`'s plugin raised on its channel.
fn raised_by(row: &ApprovalRow, from: &From) -> bool {
    row.tool == PLUGIN_CARD_TOOL
        && row.channel == from.channel
        && serde_json::from_str::<Value>(&row.arguments)
            .is_ok_and(|args| args["plugin"] == from.plugin.as_str())
}

/// What `approval/status` says of one card.
pub fn view(row: &ApprovalRow) -> Value {
    let state = match row.state.as_str() {
        PENDING | RUNNING => "still_waiting",
        SUCCEEDED => "approved",
        REJECTED => "rejected",
        CHANGES_REQUESTED => "changes_requested",
        EXPIRED => "expired",
        WITHDRAWN => "withdrawn",
        other => other,
    };
    json!({
        "approval_id": row.id,
        "state": state,
        "reason": row.reason,
        "notes": row.operator_note,
        "decided_ms": row.decided_ms,
        "finished_ms": row.finished_ms,
    })
}

/// The card `id`, if `from`'s plugin raised it on its channel.
fn own_card(access: &SessionAccess, from: &From, id: &str) -> Result<ApprovalRow, Error> {
    access
        .store
        .get_approval(id)
        .map_err(|e| Error::failed(e.to_string()))?
        .filter(|row| raised_by(row, from))
        .ok_or_else(|| {
            Error::params(format!(
                "no card {id} raised by plugin {} on {}",
                from.plugin, from.channel
            ))
        })
}

/// Take back a card this plugin raised, while it waits: it settles as
/// `withdrawn`, by the plugin. A card already decided, expired or withdrawn
/// is returned as it stands, unchanged.
async fn withdraw(access: &SessionAccess, from: &From, params: &Value) -> Result<Value, Error> {
    const WHAT: &str = "approval/withdraw";
    let map = object(params, WHAT)?;
    only(map, &["approval_id", "reason"], WHAT)?;
    let id = string(map, "approval_id", WHAT, true)?.unwrap_or_default();
    let reason = string(map, "reason", WHAT, true)?
        .unwrap_or_default()
        .trim();
    if reason.is_empty() || reason.chars().count() > MAX_WITHDRAW_REASON {
        return Err(Error::params(format!(
            "reason is 1 to {MAX_WITHDRAW_REASON} characters"
        )));
    }
    // A card past its time expires first: that is what became of it.
    access.manager.expire_approvals().await;
    own_card(access, from, id)?;
    let withdrew = access
        .store
        .withdraw_approval(id, reason)
        .map_err(|e| Error::failed(e.to_string()))?;
    let row = own_card(access, from, id)?;
    if withdrew {
        let by = format!("plugin:{}", from.plugin);
        access
            .manager
            .approval_answered(&row, WITHDRAWN, Some(&by))
            .await;
    }
    let mut out = view(&row);
    out["withdrew"] = json!(withdrew);
    Ok(out)
}

async fn status(access: &SessionAccess, from: &From, params: &Value) -> Result<Value, Error> {
    const WHAT: &str = "approval/status";
    let map = object(params, WHAT)?;
    only(map, &["approval_id", "approval_ids", "wait_secs"], WHAT)?;
    let many = map.contains_key("approval_ids");
    let ids: Vec<String> = if many {
        map["approval_ids"]
            .as_array()
            .ok_or_else(|| Error::params("approval_ids is a list of approval ids"))?
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| Error::params("approval_ids is a list of approval ids"))?
    } else {
        vec![string(map, "approval_id", WHAT, true)?
            .unwrap_or_default()
            .to_string()]
    };
    if ids.is_empty() {
        return Err(Error::params("approval_ids names no approval"));
    }
    let wait = match map.get("wait_secs") {
        None | Some(Value::Null) => MAX_WAIT_SECS,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| Error::params("wait_secs is a whole number of seconds"))?
            .min(MAX_WAIT_SECS),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        access.manager.expire_approvals().await;
        let mut rows = Vec::with_capacity(ids.len());
        for id in &ids {
            rows.push(own_card(access, from, id)?);
        }
        let decided = rows
            .iter()
            .any(|a| a.state != PENDING && a.state != RUNNING);
        if decided || tokio::time::Instant::now() >= deadline {
            let views: Vec<Value> = rows.iter().map(view).collect();
            return Ok(if many {
                json!({ "approvals": views, "still_waiting": !decided })
            } else {
                views.into_iter().next().unwrap_or(Value::Null)
            });
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn notify(
    tools: &Tools,
    access: &SessionAccess,
    from: &From,
    params: &Value,
) -> Result<Value, Error> {
    const WHAT: &str = "notify";
    let map = object(params, WHAT)?;
    only(map, &["title", "message", "path"], WHAT)?;
    let title = string(map, "title", WHAT, true)?.unwrap_or_default();
    let message = string(map, "message", WHAT, true)?.unwrap_or_default();
    let mut args = json!({
        "title": format!("{}: {title}", from.plugin),
        "message": message,
    });
    if let Some(path) = string(map, "path", WHAT, false)? {
        args["path"] = json!(path);
    }
    let ctx = CallContext::plugin(&from.plugin, &from.channel, access.manager.node_id());
    access
        .manager
        .channel_usable(&ctx.channel)
        .map_err(|e| Error::new(REFUSED, e.to_string()))?;
    super::operator::call(
        &access.store,
        &access.manager,
        &tools.cfg,
        &ctx,
        super::operator::NOTIFY,
        &args,
    )
    .await
    .map_err(Error::failed)
}

async fn call_tool(
    tools: &Tools,
    access: &SessionAccess,
    from: &From,
    params: &Value,
) -> Result<Value, Error> {
    const WHAT: &str = "tools/call";
    let map = object(params, WHAT)?;
    only(map, &["name", "arguments"], WHAT)?;
    let name = string(map, "name", WHAT, true)?.unwrap_or_default();
    let Some((callee, _)) = name.split_once(super::upstream::SEPARATOR) else {
        return Err(Error::params(format!(
            "{name} is not a plugin's tool: name it <plugin>__<tool>"
        )));
    };
    if callee == from.plugin {
        return Err(Error::new(
            REFUSED,
            format!("{name} is {}'s own tool: call it in-process", from.plugin),
        ));
    }
    let arguments = match map.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(args @ Value::Object(_)) => args.clone(),
        Some(_) => return Err(Error::params("arguments is an object")),
    };
    let ctx = CallContext::plugin(&from.plugin, &from.channel, access.manager.node_id());
    tools
        .call_for_operator(&ctx, name, &arguments)
        .await
        .map_err(|e| {
            let (code, decision) = match &e {
                OperatorCallError::Denied(_) => (REFUSED, Some("denied")),
                OperatorCallError::Asked(_) => (REFUSED, Some("asked")),
                OperatorCallError::Request(_)
                | OperatorCallError::NotOffered(_)
                | OperatorCallError::NotInstalled { .. } => (REFUSED, None),
                OperatorCallError::Unavailable
                | OperatorCallError::Unreachable(_)
                | OperatorCallError::Tool(_) => (FAILED, None),
            };
            let mut error = Error::new(code, e.to_string());
            if let Some(decision) = decision {
                error.data = Some(json!({ "decision": decision }));
            }
            if matches!(e, OperatorCallError::Tool(_)) {
                error.data = Some(json!({ "tool_error": true }));
            }
            if matches!(e, OperatorCallError::NotInstalled { .. }) {
                error.code = INVALID_PARAMS;
            }
            error
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_blocks_keep_only_what_their_format_takes() {
        let blocks = content(Some(&json!([
            { "format": "markdown", "title": "Why", "text": "**b**" },
            { "format": "diff", "text": "@@ -1 +1 @@\n-a\n+b\n" },
            { "format": "word_diff", "title": "Summary", "before": "the old words", "after": "the new words" },
            { "format": "table", "rows": [{ "key": "Owner", "value": "Josh" }] },
            { "format": "source", "language": "json", "text": "{}" },
        ])))
        .unwrap();
        assert_eq!(blocks.len(), 5);
        assert_eq!(blocks[2]["before"], "the old words");
        assert_eq!(blocks[3]["rows"][0]["value"], "Josh");
        assert_eq!(blocks[4]["language"], "json");

        for (bad, says) in [
            (json!([]), "1 to 64 blocks"),
            (json!("x"), "list of blocks"),
            (
                json!([{ "format": "html", "text": "<b>" }]),
                "not one the node renders",
            ),
            (
                json!([{ "format": "word_diff", "before": "a" }]),
                "after is required",
            ),
            (
                json!([{ "format": "markdown", "text": "a", "html": "x" }]),
                "unknown key \"html\"",
            ),
            (
                json!([{ "format": "table", "rows": [{ "key": "a", "value": 1 }] }]),
                "value must be a string",
            ),
            (
                json!([{ "format": "markdown", "text": "x".repeat(MAX_CONTENT) }]),
                "at most",
            ),
        ] {
            let error = content(Some(&bad)).unwrap_err();
            assert_eq!(error.code, INVALID_PARAMS);
            assert!(error.message.contains(says), "{says}: {}", error.message);
        }
    }

    #[test]
    fn a_card_reads_as_approved_or_not() {
        let mut row = ApprovalRow {
            id: "a1".into(),
            channel: "c".into(),
            session_id: None,
            node_id: "n".into(),
            lane: Some("plugin:hoja".into()),
            tool: PLUGIN_CARD_TOOL.into(),
            arguments: json!({ "plugin": "hoja" }).to_string(),
            request_key: "k".into(),
            title: "t".into(),
            state: PENDING.into(),
            answer_option_id: None,
            edited_arguments: None,
            result: None,
            reason: None,
            operator_note: None,
            claimed_ms: None,
            created_ms: 0,
            decided_ms: None,
            finished_ms: None,
            expires_ms: 1,
        };
        for (state, says) in [
            (PENDING, "still_waiting"),
            (RUNNING, "still_waiting"),
            (SUCCEEDED, "approved"),
            (REJECTED, "rejected"),
            (CHANGES_REQUESTED, "changes_requested"),
            (EXPIRED, "expired"),
            (WITHDRAWN, "withdrawn"),
        ] {
            row.state = state.into();
            assert_eq!(view(&row)["state"], says);
        }
        let from = From {
            plugin: "hoja".into(),
            channel: "c".into(),
            token: "t".into(),
            version: "1.0.0".into(),
            manifest: std::sync::Arc::new(
                crate::plugin::manifest::Manifest::parse(
                    "name = \"hoja\"\nversion = \"1.0.0\"\ncontract = 1\nexec = [\"bin/hoja\"]\n",
                )
                .unwrap(),
            ),
        };
        assert!(raised_by(&row, &from));
        let other = From {
            plugin: "sql-review".into(),
            ..from.clone()
        };
        assert!(!raised_by(&row, &other));
        let elsewhere = From {
            channel: "d".into(),
            ..from
        };
        assert!(!raised_by(&row, &elsewhere));
    }
}
