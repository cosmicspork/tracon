//! Node tools, reached over MCP's HTTP transport: the servers `session/new`
//! names, each listed once at session start and offered to the model beside
//! the built-in tools.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Context, Output, Tool};

const PROTOCOL_VERSION: &str = "2025-06-18";
/// Function names the OpenAI wire accepts: `[A-Za-z0-9_-]{1,64}`.
const MAX_NAME: usize = 64;

pub struct Server {
    name: String,
    url: String,
    headers: Vec<(String, String)>,
    http: reqwest::Client,
    next_id: AtomicU64,
}

impl Server {
    /// From one entry of `session/new`'s `mcpServers`:
    /// `{"type": "http", "name", "url", "headers": [{"name", "value"}]}`.
    pub fn from_config(config: &Value) -> Result<Self, String> {
        let kind = config["type"].as_str().unwrap_or("http");
        if kind != "http" {
            return Err(format!(
                "MCP server type `{kind}` is not supported; only http"
            ));
        }
        let name = config["name"]
            .as_str()
            .ok_or("MCP server without a name")?
            .to_string();
        let url = config["url"]
            .as_str()
            .ok_or_else(|| format!("MCP server {name} without a url"))?
            .to_string();
        let headers = config["headers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| {
                Some((
                    h["name"].as_str()?.to_string(),
                    h["value"].as_str()?.to_string(),
                ))
            })
            .collect();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            name,
            url,
            headers,
            http,
            next_id: AtomicU64::new(1),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self.post(&body).await?;
        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{} {method}: HTTP {status}: {text}", self.name));
        }
        let message = single_message(&text)
            .ok_or_else(|| format!("{} {method}: unreadable response: {text}", self.name))?;
        if let Some(e) = message.get("error") {
            return Err(format!(
                "{} {method}: {}",
                self.name,
                e["message"].as_str().unwrap_or(&e.to_string())
            ));
        }
        Ok(message["result"].clone())
    }

    async fn notify(&self, method: &str) -> Result<(), String> {
        self.post(&json!({"jsonrpc": "2.0", "method": method}))
            .await
            .map(|_| ())
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response, String> {
        let mut request = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .json(body);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        request
            .send()
            .await
            .map_err(|e| format!("{}: {e}", self.name))
    }

    /// Initializes the server and lists its tools.
    pub async fn connect(self) -> Result<Vec<Box<dyn Tool>>, String> {
        let server = Arc::new(self);
        server
            .call(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "tracon-agent", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        server.notify("notifications/initialized").await?;
        let listed = server.call("tools/list", json!({})).await?;
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        for t in listed["tools"].as_array().into_iter().flatten() {
            let Some(remote) = t["name"].as_str() else {
                continue;
            };
            tools.push(Box::new(McpTool {
                server: server.clone(),
                remote: remote.to_string(),
                name: function_name(&server.name, remote),
                description: t["description"].as_str().unwrap_or_default().to_string(),
                parameters: match &t["inputSchema"] {
                    Value::Object(_) => t["inputSchema"].clone(),
                    _ => json!({"type": "object", "properties": {}}),
                },
            }));
        }
        Ok(tools)
    }
}

/// The node answers with one JSON body; a server that streams answers with
/// SSE, and the first `data:` holding a response is the answer.
fn single_message(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Some(v);
    }
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .find(|v| v.get("result").is_some() || v.get("error").is_some())
}

/// `mcp__<server>__<tool>`, cut to the characters and length function names
/// allow. Claude Code's spelling, which is the one the node recognises its own
/// tools by.
pub fn function_name(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{server}__{tool}");
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MAX_NAME)
        .collect()
}

struct McpTool {
    server: Arc<Server>,
    remote: String,
    name: String,
    description: String,
    parameters: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    async fn run(&self, _cx: &Context, input: Value) -> Output {
        let result = match self
            .server
            .call(
                "tools/call",
                json!({"name": self.remote, "arguments": input}),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return Output::error(e),
        };
        let text = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| match c["type"].as_str() {
                Some("text") => c["text"].as_str().unwrap_or_default().to_string(),
                _ => c.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.is_empty() {
            result
                .get("structuredContent")
                .map(Value::to_string)
                .unwrap_or_default()
        } else {
            text
        };
        Output {
            content: text,
            is_error: result["isError"].as_bool().unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};

    #[test]
    fn function_names_fit_the_wire() {
        assert_eq!(
            function_name("tracon", "submit_review"),
            "mcp__tracon__submit_review"
        );
        assert_eq!(function_name("my server", "a.b"), "mcp__my_server__a_b");
        assert_eq!(function_name("s", &"x".repeat(100)).len(), MAX_NAME);
    }

    #[test]
    fn reads_json_or_sse() {
        assert_eq!(single_message(r#"{"result":1}"#).unwrap()["result"], 1);
        let sse =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        assert_eq!(single_message(sse).unwrap()["result"]["ok"], true);
    }

    #[tokio::test]
    async fn lists_and_calls_tools_with_the_bearer() {
        async fn handle(headers: axum::http::HeaderMap, Json(body): Json<Value>) -> Json<Value> {
            assert_eq!(headers["authorization"], "Bearer tok");
            let id = body["id"].clone();
            Json(match body["method"].as_str().unwrap() {
                "initialize" => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": PROTOCOL_VERSION}})
                }
                "notifications/initialized" => json!({}),
                "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                    {"name": "echo", "description": "Echo", "inputSchema": {"type": "object", "properties": {"x": {"type": "string"}}}}
                ]}}),
                "tools/call" => {
                    let x = body["params"]["arguments"]["x"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": x}], "isError": x == "bad"}})
                }
                other => panic!("unexpected {other}"),
            })
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/mcp/s1", post(handle)))
                .await
                .unwrap()
        });
        let server = Server::from_config(&json!({
            "type": "http", "name": "tracon", "url": format!("http://{addr}/mcp/s1"),
            "headers": [{"name": "Authorization", "value": "Bearer tok"}]
        }))
        .unwrap();
        let tools = server.connect().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__tracon__echo");
        let dir = tempfile::tempdir().unwrap();
        let cx = Context::new(dir.path()).unwrap();
        assert_eq!(
            tools[0].run(&cx, json!({"x": "hi"})).await,
            Output::ok("hi")
        );
        assert!(tools[0].run(&cx, json!({"x": "bad"})).await.is_error);
    }
}
