//! The socket the node serves each plugin instance, for calls the other way:
//! from the plugin to the node.
//!
//! A plugin whose manifest names any `node_methods` is started with
//! `--node-socket <path>` beside its own `--socket`. The node listens there
//! for that one instance, in the same private 0700 directory, for as long as
//! the start it was made for runs; the next start gets a fresh socket.
//!
//! Each request is one JSON-RPC 2.0 call, `POST /` with a JSON body, sent
//! with `Authorization: Bearer <TRACON_PLUGIN_TOKEN>`: the token the
//! instance was started with. Another token, an earlier start's included,
//! is refused with HTTP 401 before anything is read. The node answers only
//! the methods the manifest names; any other is `-32601`. What each method
//! does is the [`Host`]'s (`crate::mcp::plugin_host`), which knows the
//! instance it answers for by the socket it was reached on, never by
//! anything the plugin says.

use std::{path::PathBuf, sync::Arc};

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::manifest::Manifest;

/// The largest request body the node reads from a plugin.
pub const MAX_BODY: usize = 1 << 20;

/// The method is not one the node has, or not one the manifest names.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The request is not JSON-RPC 2.0.
pub const INVALID_REQUEST: i64 = -32600;
/// The params are not what the method takes.
pub const INVALID_PARAMS: i64 = -32602;
/// The token is not this instance's.
pub const UNAUTHORIZED: i64 = -32001;
/// The node refused: by policy, by the operator's entry, or because the
/// call would be asked and a plugin's call is never held.
pub const REFUSED: i64 = -32003;
/// The node could not do it: a plugin it called failed, a store error.
pub const FAILED: i64 = -32000;

/// The instance a request reached the node from: what its socket was made
/// for, never what the request says.
#[derive(Debug, Clone)]
pub struct From {
    pub plugin: String,
    pub channel: String,
    /// The instance's `TRACON_PLUGIN_TOKEN`.
    pub token: Arc<str>,
    pub version: String,
    pub manifest: Arc<Manifest>,
}

/// Why a method failed, as a JSON-RPC error.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(FAILED, message)
    }
}

/// What answers a plugin's calls to the node: the node's tools.
#[async_trait::async_trait]
pub trait Host: Send + Sync {
    async fn call(&self, from: &From, method: &str, params: Value) -> Result<Value, Error>;
}

/// What a listening socket answers with.
#[derive(Clone)]
struct Served {
    from: From,
    /// The sha256 of the token, which a presented token's is compared to,
    /// so the comparison's time says nothing about the token.
    instance: String,
    host: std::sync::Weak<dyn Host>,
}

/// One instance's node socket, listening until it is dropped or stopped.
pub struct Listening {
    pub path: PathBuf,
    stop: CancellationToken,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.cancel();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listen on `path` for `from`, answering through `host`. The socket is
/// made 0600 before anything can reach it through its 0700 directory.
pub fn listen(
    path: PathBuf,
    from: From,
    host: std::sync::Weak<dyn Host>,
) -> std::io::Result<Listening> {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let served = Served {
        instance: super::context::instance_of(&from.token),
        from,
        host,
    };
    let app = Router::new()
        .route("/", post(answer))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(served);
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    tokio::spawn(async move {
        let serving = axum::serve(listener, app).with_graceful_shutdown(stopped.cancelled_owned());
        if let Err(error) = serving.await {
            tracing::warn!(%error, "a plugin's node socket stopped");
        }
    });
    Ok(Listening { path, stop })
}

fn reply(id: &Value, outcome: Result<Value, Error>) -> Value {
    match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => {
            let mut body = json!({ "code": error.code, "message": error.message });
            if let Some(data) = error.data {
                body["data"] = data;
            }
            json!({ "jsonrpc": "2.0", "id": id, "error": body })
        }
    }
}

/// The bearer token a request presents.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

async fn answer(
    State(served): State<Served>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let presented = bearer(&headers).map(super::context::instance_of);
    if presented.as_deref() != Some(served.instance.as_str()) {
        let error = Error::new(
            UNAUTHORIZED,
            "this socket answers only the instance it was made for: send its \
             TRACON_PLUGIN_TOKEN as a bearer token",
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(reply(&Value::Null, Err(error))),
        )
            .into_response();
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            let error = Error::new(INVALID_REQUEST, format!("not JSON: {e}"));
            return Json(reply(&Value::Null, Err(error))).into_response();
        }
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str();
    let (Some(method), true, false) = (method, request["jsonrpc"] == "2.0", id.is_null()) else {
        let error = Error::new(
            INVALID_REQUEST,
            "a JSON-RPC 2.0 request with an id and a method",
        );
        return Json(reply(&id, Err(error))).into_response();
    };
    let from = &served.from;
    let outcome = if !from.manifest.calls_node(method) {
        let known = super::manifest::NODE_METHODS
            .iter()
            .any(|(m, _)| *m == method);
        Err(Error::new(
            METHOD_NOT_FOUND,
            if known {
                format!(
                    "{method} is not in {}'s node_methods, so the node does not answer it",
                    from.plugin
                )
            } else {
                format!("the node has no method {method}")
            },
        ))
    } else {
        match served.host.upgrade() {
            Some(host) => {
                let params = request.get("params").cloned().unwrap_or(json!({}));
                host.call(from, method, params).await
            }
            None => Err(Error::failed("the node is shutting down")),
        }
    };
    Json(reply(&id, outcome)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait::async_trait]
    impl Host for Echo {
        async fn call(&self, from: &From, method: &str, params: Value) -> Result<Value, Error> {
            if method == "notify" {
                return Err(Error {
                    code: REFUSED,
                    message: "no".into(),
                    data: Some(json!({ "why": "test" })),
                });
            }
            Ok(
                json!({ "plugin": from.plugin, "channel": from.channel, "method": method, "params": params }),
            )
        }
    }

    async fn post(path: &std::path::Path, token: Option<&str>, body: &str) -> (u16, Value) {
        let http = reqwest::Client::builder()
            .unix_socket(path)
            .build()
            .unwrap();
        let mut request = http
            .post("http://node/")
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    }

    #[tokio::test]
    async fn a_request_is_answered_only_with_the_instances_token_and_declared_methods() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("n.sock");
        let manifest = Manifest::parse(
            "name = \"p\"\nversion = \"1.0.0\"\ncontract = 1\nexec = [\"bin/p\"]\n\
             node_methods = [\"approval/request\", \"notify\"]\n",
        )
        .unwrap();
        let host: Arc<dyn Host> = Arc::new(Echo);
        let listening = listen(
            path.clone(),
            From {
                plugin: "p".into(),
                channel: "personal".into(),
                token: "s3cret".into(),
                version: "1.0.0".into(),
                manifest: Arc::new(manifest),
            },
            Arc::downgrade(&host),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let call = r#"{"jsonrpc":"2.0","id":7,"method":"approval/request","params":{"title":"t"}}"#;

        for token in [None, Some("wrong"), Some("s3cret ")] {
            let (status, body) = post(&path, token, call).await;
            if token == Some("s3cret ") {
                // Trailing space is trimmed: it is the same token.
                assert_eq!(status, 200, "{body}");
                continue;
            }
            assert_eq!(status, 401, "{token:?}");
            assert_eq!(body["error"]["code"], UNAUTHORIZED);
        }
        let (status, body) = post(&path, Some("s3cret"), call).await;
        assert_eq!(status, 200);
        assert_eq!(body["id"], 7);
        assert_eq!(body["result"]["plugin"], "p");
        assert_eq!(body["result"]["channel"], "personal");
        assert_eq!(body["result"]["params"]["title"], "t");

        // Declared, and the host's refusal carries its data.
        let (_, body) = post(
            &path,
            Some("s3cret"),
            r#"{"jsonrpc":"2.0","id":"a","method":"notify"}"#,
        )
        .await;
        assert_eq!(body["error"]["code"], REFUSED);
        assert_eq!(body["error"]["data"]["why"], "test");
        // A node method the manifest leaves out, and one the node lacks.
        let (_, body) = post(
            &path,
            Some("s3cret"),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#,
        )
        .await;
        assert_eq!(body["error"]["code"], METHOD_NOT_FOUND);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not in p's node_methods"));
        let (_, body) = post(
            &path,
            Some("s3cret"),
            r#"{"jsonrpc":"2.0","id":1,"method":"shell"}"#,
        )
        .await;
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no method shell"));
        // Not JSON-RPC: no id, or not JSON at all.
        let (_, body) = post(
            &path,
            Some("s3cret"),
            r#"{"jsonrpc":"2.0","method":"notify"}"#,
        )
        .await;
        assert_eq!(body["error"]["code"], INVALID_REQUEST);
        let (_, body) = post(&path, Some("s3cret"), "{").await;
        assert_eq!(body["error"]["code"], INVALID_REQUEST);

        // Dropped, it stops listening and its file goes.
        drop(listening);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!path.exists());
    }
}
