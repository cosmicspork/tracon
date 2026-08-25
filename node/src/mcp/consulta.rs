//! consulta as the node's first brokered tool.
//!
//! Why this one first: one credential, read-only by construction, the smallest
//! blast radius of anything the broker will hold, and it exercises the whole
//! path — tool call, channel binding, node-side guard, credential injection,
//! external call, result — before `gh` depends on it.
//!
//! The node refuses before spawning; consulta's own guard refuses again inside
//! the sidecar. Two independent checks, now on opposite sides of a privilege
//! boundary.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    broker::{guard, Broker},
    config::Config,
    mcp::CallContext,
};

pub const CREDENTIAL: &str = "consulta";
pub const QUERY: &str = "query";
pub const DESCRIBE: &str = "describe";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": QUERY,
            "description": "Run a read-only SQL query. Only a single SELECT or WITH statement is \
                            accepted; anything else is refused before it reaches the database. \
                            Bind values with :name and pass them in params.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single SELECT or WITH statement." },
                    "params": { "type": "object", "description": "Bind values for :name placeholders." },
                    "limit": { "type": "integer", "description": "Maximum rows; 0 means uncapped." },
                },
                "required": ["sql"],
            },
        }),
        json!({
            "name": DESCRIBE,
            "description": "List the columns of a table, with types and nullability.",
            "inputSchema": {
                "type": "object",
                "properties": { "table": { "type": "string" } },
                "required": ["table"],
            },
        }),
    ]
}

pub async fn call(
    broker: &Arc<Broker>,
    cfg: &Arc<Config>,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    // Channel binding first: which channel may use which connection.
    let env = broker
        .env_for(CREDENTIAL, &ctx.channel)
        .map_err(|e| e.to_string())?;

    let mut argv: Vec<String> = Vec::new();
    if name == DESCRIBE {
        let table = args
            .get("table")
            .and_then(Value::as_str)
            .ok_or("describe needs a table")?;
        // describe generates its data-dictionary SELECT inside the sidecar, so
        // the node cannot run that SQL through `assert_read_only` the way it does
        // a query. The node-side check for this path is therefore the identifier
        // itself: a bare table name, so nothing can be injected into the
        // generated statement. consulta's own guard is the second check.
        let valid = !table.is_empty()
            && !table.starts_with('.')
            && !table.ends_with('.')
            && !table.contains("..")
            && table
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$');
        if !valid {
            return Err(format!("{table:?} is not a table name"));
        }
        argv.push("--describe".into());
        argv.push(table.into());
    } else {
        let sql = args
            .get("sql")
            .and_then(Value::as_str)
            .ok_or("query needs sql")?;
        // The node's own check, before anything is spawned.
        guard::assert_read_only(sql).map_err(|e| format!("refused: {e}"))?;
        argv.push("--sql".into());
        argv.push(sql.into());
        if let Some(params) = args.get("params").and_then(Value::as_object) {
            for (k, v) in params {
                let value = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                argv.push("--param".into());
                argv.push(format!("{k}={value}"));
            }
        }
        if let Some(limit) = args.get("limit").and_then(Value::as_i64) {
            argv.push("--limit".into());
            argv.push(limit.to_string());
        }
    }
    argv.push("--format".into());
    argv.push("json".into());
    argv.push("--quiet".into());

    run_sidecar(cfg, env, argv, &ctx.session_id).await
}

/// The sidecar runs on the node's side of the boundary with the credential in
/// its environment. It is not reachable from the harness; only its result is.
async fn run_sidecar(
    cfg: &Arc<Config>,
    env: std::collections::BTreeMap<String, String>,
    argv: Vec<String>,
    session_id: &str,
) -> Result<Value, String> {
    let mut cmd = tokio::process::Command::new(&cfg.consulta.command);
    cmd.args(&cfg.consulta.args)
        .args(&argv)
        // A clean environment: the sidecar gets the credential and nothing
        // inherited that might contain another one.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .envs(env)
        .kill_on_drop(true);

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(cfg.consulta.timeout_secs),
        cmd.output(),
    )
    .await
    .map_err(|_| "the query timed out".to_string())?
    .map_err(|e| format!("could not run the query tool: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        tracing::warn!(session = %session_id, "brokered query refused or failed");
        // consulta reports refusals and errors on stderr with exit 2. Pass the
        // reason back so the agent can correct itself, not a bare failure.
        let reason = stderr
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("the query failed")
            .trim();
        return Err(reason.to_string());
    }
    serde_json::from_str::<Value>(stdout.trim()).map_err(|_| {
        format!(
            "the query tool returned unreadable output: {}",
            stdout.trim()
        )
    })
}
