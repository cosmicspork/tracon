//! Asking before egress, rather than refusing it.
//!
//! Inside the boundary a session's writes and commands run unasked; what
//! leaves it is the operator's to decide. A session reaches the hosts its own
//! grant opens (`environment::session_grant`). A CONNECT to any other host is
//! still refused — a connection cannot be held open while a person decides —
//! but the refusal now raises a card naming the session and the host, and the
//! refusal's reason line tells the agent that an ask is pending and to retry
//! once it is answered. `request_egress` asks first, and waits, for a host the
//! agent knows it needs before it tries.
//!
//! An ask is an approval (`store::approvals::EGRESS_TOOL`), so it waits in the
//! operator's one queue and `approval_status` reads it. Allowing it opens the
//! host to the session's own grant — for a few minutes, for the rest of the
//! session, or for the rest of the session and every later one through the
//! repository's `egress` — and runs nothing. Podman only, as grants are.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{
    mcp::{approvals, CallContext, SessionAccess},
    session::Manager,
    store::approvals::{ApprovalRow, EGRESS_TOOL, PENDING, REJECTED},
};

pub const REQUEST: &str = EGRESS_TOOL;

/// How long "Allow once" keeps a host open: the retry the refusal asked
/// for, with room for a package manager's handful of connections, and no
/// longer.
pub const ONCE: Duration = Duration::from_secs(10 * 60);

const MAX_WHY_CHARS: usize = 500;

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": REQUEST,
        "description": format!(
            "Ask the operator to open a host to this session's network, and wait for the \
             answer. Hosts your repository does not open are refused (403), and a refused \
             connection already asks the operator: its reason says so. Call this when you \
             know a host you need before you try it, or to wait for the answer to such a \
             refusal. Blocks for up to {} seconds and returns `reachable` once the host is \
             open, `still_waiting` while the operator has not answered (call again with the \
             same host), or `rejected` with their reason — then do without it, and do not \
             look for another way to fetch it. Only HTTPS on port 443 is ever proxied.",
            super::wait::MAX_WAIT_SECS
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "host": { "type": "string", "description": "The host name, such as `pypi.org`." },
                "why": { "type": "string", "description": "What you need it for, in a sentence the operator will read." },
                "wait_secs": {
                    "type": "integer",
                    "description": format!(
                        "How long to block, up to {}. 0 asks and returns at once.",
                        super::wait::MAX_WAIT_SECS
                    ),
                },
            },
            "required": ["host"],
        },
    })]
}

/// A host as the proxy compares it: lowercase, without a scheme, a path or
/// port 443. Anything that could not be a host name on 443 is refused here
/// rather than turned into a card nobody can usefully allow.
pub fn normalize_host(raw: &str) -> Result<String, String> {
    let mut host = raw.trim().to_ascii_lowercase();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = host.strip_prefix(scheme) {
            host = rest.to_string();
        }
    }
    if let Some((name, _)) = host.split_once('/') {
        host = name.to_string();
    }
    if let Some((name, port)) = host.rsplit_once(':') {
        if port != "443" {
            return Err(format!(
                "only HTTPS on port 443 is proxied; {raw:?} names port {port}"
            ));
        }
        host = name.to_string();
    }
    let valid = !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    if !valid {
        return Err(format!("{raw:?} is not a host name"));
    }
    Ok(host)
}

/// One ask per session and host on a channel, whoever raised it.
fn request_key(session_id: &str, host: &str) -> String {
    crate::corpus::hash_body(&format!("{REQUEST}\u{1f}{session_id}\u{1f}{host}"))
}

/// Where an ask stands.
pub enum Asked {
    /// Waiting on the operator; `true` when this call raised it, so the
    /// caller announces it.
    Pending(ApprovalRow, bool),
    /// The operator already refused this host to this session. A refused
    /// connection does not ask again: a package manager retries, and the
    /// operator answered once.
    Declined(ApprovalRow),
}

/// Find or raise the operator's ask for `host` on behalf of `session_id`.
/// `again` asks anew after a refusal: an agent's explicit request may say
/// something the first did not, where a retried connection says nothing new.
pub fn ask(
    manager: &Manager,
    session_id: &str,
    host: &str,
    why: Option<&str>,
    again: bool,
) -> Result<Asked, String> {
    // Package managers open connections in parallel, so refusals for one host
    // arrive together: finding the ask and raising it is one step, or each
    // would raise its own card.
    static ASKING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _asking = ASKING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let store = manager.store();
    let session = store
        .get_session(session_id)
        .map_err(|e| e.to_string())?
        .ok_or("this session is gone")?;
    let key = request_key(session_id, host);
    let now = crate::store::now_ms();
    match store
        .latest_approval(&session.channel, &key)
        .map_err(|e| e.to_string())?
    {
        Some(open) if open.state == PENDING && open.expires_ms > now => {
            return Ok(Asked::Pending(open, false))
        }
        Some(refused) if refused.state == REJECTED && !again => {
            return Ok(Asked::Declined(refused))
        }
        _ => {}
    }
    let why = why
        .map(str::trim)
        .filter(|why| !why.is_empty())
        .map(|why| why.chars().take(MAX_WHY_CHARS).collect::<String>());
    let short = session_id.get(..8).unwrap_or(session_id);
    let title = match &why {
        Some(why) => format!("reach {host} from session {short} · {why}"),
        None => format!("reach {host} from session {short}"),
    };
    let mut arguments = json!({ "host": host });
    if let Some(why) = &why {
        arguments["why"] = json!(why);
    }
    let row = ApprovalRow {
        id: uuid::Uuid::now_v7().to_string(),
        channel: session.channel.clone(),
        session_id: Some(session_id.to_string()),
        node_id: manager.node_id().to_string(),
        lane: None,
        tool: REQUEST.to_string(),
        arguments: arguments.to_string(),
        request_key: key,
        title,
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
        expires_ms: now + (manager.cfg().session.approval_expiry_secs.max(1) as i64) * 1000,
    };
    store.insert_approval(&row).map_err(|e| e.to_string())?;
    Ok(Asked::Pending(row, true))
}

/// What a refused CONNECT is told, once the operator has been asked: the
/// status line's reason, so it is the line a package manager prints. Plain
/// ASCII, as a reason phrase must be.
pub fn refusal(host: &str, asked: &Asked) -> String {
    match asked {
        Asked::Pending(row, _) => format!(
            "{host} is not open to this session; the operator has been asked (approval {}). \
             Retry once it is answered: request_egress waits for the answer",
            row.id
        ),
        Asked::Declined(_) => {
            format!("{host} is not open to this session; the operator declined it. Do without it")
        }
    }
}

/// `request_egress`, for a session the node runs.
pub async fn call(
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let manager = &access.manager;
    let Some(session_id) = ctx.session_id() else {
        return Err(
            "a harness you run yourself is not behind the node's proxy; nothing here opens its \
             network"
                .into(),
        );
    };
    let host = normalize_host(args.get("host").and_then(Value::as_str).unwrap_or(""))?;
    let grants = manager
        .egress_grants()
        .filter(|grants| grants.holds_session(session_id))
        .ok_or(
            "this session holds no egress grant of its own to open: only a Claude Code session \
             on the Podman runtime reaches the network through one",
        )?;
    if grants.session_allows(session_id, &host) {
        return Ok(json!({ "host": host, "state": "reachable" }));
    }
    let why = args.get("why").and_then(Value::as_str);
    let approval = match ask(manager, session_id, &host, why, true)? {
        Asked::Pending(row, created) => {
            if created {
                manager.approval_requested(&row).await;
            }
            row
        }
        Asked::Declined(row) => row,
    };
    let mut status = approvals::status(
        access,
        ctx,
        &json!({ "approval_id": approval.id, "wait_secs": args.get("wait_secs") }),
    )
    .await?;
    status["host"] = json!(host);
    if grants.session_allows(session_id, &host) {
        status["state"] = json!("reachable");
    }
    Ok(status)
}

/// What the operator's answer opens: the scope it was given, and until when.
pub struct Opened {
    pub scope: &'static str,
    pub until: Option<Instant>,
}

/// The scope an egress answer opens, for an option the operator picked;
/// `None` for an answer that opens nothing.
pub fn scope_of(option_id: &str) -> Option<Opened> {
    use crate::store::approvals::{OPTION_ALLOW_REPO, OPTION_ALLOW_SESSION};
    match option_id {
        crate::adapter::types::OPTION_ALLOW_ONCE => Some(Opened {
            scope: "once",
            until: Some(Instant::now() + ONCE),
        }),
        OPTION_ALLOW_SESSION => Some(Opened {
            scope: "session",
            until: None,
        }),
        OPTION_ALLOW_REPO => Some(Opened {
            scope: "repository",
            until: None,
        }),
        _ => None,
    }
}

/// The host an egress approval asks for.
pub fn host_of(approval: &ApprovalRow) -> Result<String, String> {
    let arguments: Value = serde_json::from_str(&approval.arguments).map_err(|e| e.to_string())?;
    normalize_host(arguments["host"].as_str().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_named_the_way_the_proxy_compares_it() {
        for (raw, host) in [
            ("pypi.org", "pypi.org"),
            ("  PyPI.org ", "pypi.org"),
            (
                "https://files.pythonhosted.org/packages/x",
                "files.pythonhosted.org",
            ),
            ("registry.npmjs.org:443", "registry.npmjs.org"),
        ] {
            assert_eq!(normalize_host(raw).unwrap(), host, "{raw}");
        }
        for bad in [
            "",
            "localhost",
            "pypi.org:80",
            "evil.com\r\nx",
            "a..b",
            "-x.com",
            "*.github.com",
            "pypi.org:8443",
        ] {
            assert!(normalize_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_refusal_reads_as_a_reason_phrase() {
        let row = ApprovalRow {
            id: "a1".into(),
            channel: "work".into(),
            session_id: Some("s1".into()),
            node_id: "n1".into(),
            lane: None,
            tool: REQUEST.into(),
            arguments: r#"{"host":"pypi.org"}"#.into(),
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
            expires_ms: 0,
        };
        let said = refusal("pypi.org", &Asked::Pending(row, true));
        assert!(said.contains("approval a1"), "{said}");
        assert!(hyper::ext::ReasonPhrase::try_from(said.as_bytes()).is_ok());
    }
}
