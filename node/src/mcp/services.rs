//! `service_start` and `service_status`: a catalogue service beside the
//! session, asked for by name.
//!
//! Unlike `show_work`, these go through the policy bundle like any other
//! tool, with the service's name as the argument a rule can scope: the
//! shipped bundle runs a browser unattended and asks about anything else.

use std::time::Duration;

use serde_json::{json, Value};

use crate::{
    config::Config,
    mcp::{wait::wait_secs, CallContext, SessionAccess},
    session::state::event_kind as ek,
    sidecars,
};

pub const START: &str = "service_start";
pub const STATUS: &str = "service_status";

pub fn definitions(cfg: &Config) -> Vec<Value> {
    let offered = sidecars::describe(cfg);
    let names: Vec<&str> = cfg.service.iter().map(|s| s.name.as_str()).collect();
    let name = json!({
        "type": "string",
        "description": "The service's name in the node's catalogue.",
        "enum": names,
    });
    let wait = json!({
        "type": "integer",
        "minimum": 0,
        "description": "Seconds to wait for it to become ready (capped). Default 30.",
    });
    vec![
        json!({
            "name": START,
            "description": format!(
                "Start a service from the node's catalogue beside this session, by name, and \
                 wait for it to answer. It shares this session's network: reach it on \
                 127.0.0.1 at its port, and it reaches what this session can and nothing \
                 more. It runs until the session ends; starting it again is harmless. A \
                 browser exposes the Chrome DevTools Protocol, which Playwright's \
                 `connectOverCDP` or a browser MCP can drive; screenshots it takes can be \
                 shown with show_work. Offered here: {offered}."
            ),
            "inputSchema": {
                "type": "object",
                "properties": { "name": name, "wait_secs": wait },
                "required": ["name"],
            },
        }),
        json!({
            "name": STATUS,
            "description": "Whether a service started with service_start is ready, waiting \
                            for it if it is still starting: not_started, starting, ready or \
                            failed (with why).",
            "inputSchema": {
                "type": "object",
                "properties": { "name": name, "wait_secs": wait },
                "required": ["name"],
            },
        }),
    ]
}

pub async fn call(
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let session_id = ctx
        .session_id()
        .ok_or("services run beside a session this node started")?;
    let service = args
        .get("name")
        .and_then(Value::as_str)
        .ok_or("name is required")?;
    let session = access
        .store
        .get_session(session_id)
        .map_err(|e| e.to_string())?
        .ok_or("no such session")?;
    let manager = &access.manager;
    let runner = manager
        .backend()
        .runner_for(&session.harness_id, Vec::new());
    let record = |payload: Value| manager.record_event(session_id, ek::SERVICE, payload);
    let cx = sidecars::Context {
        cfg: manager.cfg(),
        store: &access.store,
        runner: runner.as_ref(),
        session: &session,
        record: &record,
    };
    let wait = Duration::from_secs(if args.get("wait_secs").is_some() {
        wait_secs(args)
    } else {
        30
    });
    match name {
        START => sidecars::start(&cx, service, wait).await,
        _ => sidecars::status(&cx, service, wait).await,
    }
}
