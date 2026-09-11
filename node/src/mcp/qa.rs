//! Candidate-bound QA and prototype tools. These expose declarations and
//! identities only: target URLs, images, shell lines, paths, and credential
//! values remain operator configuration or runtime-owned state.

use serde_json::{json, Value};

use crate::{
    mcp::{CallContext, SessionAccess, Tools},
    qa::{self, service::{self, QaAccess}},
};

pub const DEPLOY: &str = "qa_deploy";
pub const BROWSER_VERIFY: &str = "qa_browser_verify";
pub const PROTOTYPE_BUILD: &str = "prototype_build";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": DEPLOY,
            "description": "Deploy an immutable candidate SHA to one operator-configured private QA target. Requires an active exact deploy grant for that target; never accepts a production environment, image, URL, or forge credential.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "candidate_id": { "type": "string" },
                    "target": { "type": "string" },
                },
                "required": ["candidate_id", "target"],
            },
        }),
        json!({
            "name": BROWSER_VERIFY,
            "description": "Run a declarative browser proof against one existing QA deployment. Network access is restricted to the target's configured origins. A dedicated test-account grant is required only when a fill requests a configured credential field.",
            "inputSchema": browser_schema(),
        }),
        json!({
            "name": PROTOTYPE_BUILD,
            "description": "Build a sandboxed HTML prototype from the candidate owner's prepared repository snapshot using the operator-configured immutable recipe. It accepts neither a host path nor a command.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "candidate_id": { "type": "string" } },
                "required": ["candidate_id"],
            },
        }),
    ]
}

pub async fn call(
    tools: &Tools,
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let qa_access = QaAccess {
        store: &access.store,
        manager: &access.manager,
        cfg: &tools.cfg,
        broker: &tools.broker,
        http: &tools.http,
        policy: &tools.policy,
        node_id: &ctx.node_id,
        requester_session_id: Some(&ctx.session_id),
        requester_channel: Some(&ctx.channel),
    };
    match name {
        DEPLOY => {
            let request: qa::DeployRequest = serde_json::from_value(args.clone())
                .map_err(|error| format!("qa_deploy arguments are invalid: {error}"))?;
            Ok(json!(service::deploy(&qa_access, request).await?))
        }
        BROWSER_VERIFY => {
            let request: qa::BrowserRequest = serde_json::from_value(args.clone())
                .map_err(|error| format!("qa_browser_verify arguments are invalid: {error}"))?;
            Ok(json!(service::browser_verify(&qa_access, request).await?))
        }
        PROTOTYPE_BUILD => {
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Request {
                candidate_id: String,
            }
            let request: Request = serde_json::from_value(args.clone())
                .map_err(|error| format!("prototype_build arguments are invalid: {error}"))?;
            Ok(json!(service::build_prototype(&qa_access, &request.candidate_id).await?))
        }
        other => Err(format!("no QA tool named {other}")),
    }
}

fn browser_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "deployment_id": { "type": "string" },
            "scenario": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "start_path": { "type": "string", "description": "Absolute path on the configured QA origin" },
                    "steps": {
                        "type": "array",
                        "maxItems": 32,
                        "items": {
                            "oneOf": [
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "navigate" }, "path": { "type": "string" } }, "required": ["kind", "path"] },
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "click" }, "selector": { "type": "string" } }, "required": ["kind", "selector"] },
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "fill" }, "selector": { "type": "string" }, "value": { "type": "string" }, "credential_env": { "type": "string" } }, "required": ["kind", "selector"] },
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "wait_for" }, "selector": { "type": "string" } }, "required": ["kind", "selector"] }
                            ]
                        }
                    },
                    "assertions": {
                        "type": "array",
                        "maxItems": 32,
                        "items": {
                            "oneOf": [
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "url_path_is" }, "path": { "type": "string" } }, "required": ["kind", "path"] },
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "title_contains" }, "text": { "type": "string" } }, "required": ["kind", "text"] },
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "text_visible" }, "selector": { "type": "string" }, "text": { "type": "string" } }, "required": ["kind", "selector", "text"] },
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "element_count" }, "selector": { "type": "string" }, "count": { "type": "integer", "minimum": 0 } }, "required": ["kind", "selector", "count"] },
                                { "type": "object", "additionalProperties": false, "properties": { "kind": { "const": "screenshot" }, "label": { "type": "string" } }, "required": ["kind", "label"] }
                            ]
                        }
                    },
                },
                "required": ["start_path", "steps", "assertions"],
            },
        },
        "required": ["deployment_id", "scenario"],
    })
}
