//! The model gateway: the harness sends its ordinary provider request to
//! `http://tracon-gw:<forward>/model/<provider>/…` carrying a placeholder key
//! (its session token), and the node swaps in the real credential and forwards
//! over TLS. No interception — the request is the harness's own, so the shape
//! a subscription token demands is preserved. This is the enforcement point for
//! provider bindings and the counting point for usage: every model call passes
//! through here, so cost is measured where it happens rather than reported by
//! the harness.

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_core::Stream;
use serde_json::{json, Value};

use crate::session::state::event_kind as ek;

use crate::{
    broker::Injection,
    config::{Config, Provider, SHAPE_ANTHROPIC, SHAPE_OPENAI},
    http::api::AppState,
    store::{now_ms, UsageRow},
};

/// The beta flag Anthropic's subscription tokens are issued under. Unverified
/// against a live token (see `docs/reference/phase-4-notes.md`); kept as data
/// so the next observation changes one line.
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// What a harness needs to reach the gateway: environment for the providers
/// that honour one, and a `models.json` for the ones that only read a
/// provider override (omp's `openai`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wiring {
    pub env: Vec<(String, String)>,
    pub models_json: String,
}

/// The base URL for one provider as the harness sees it.
pub fn base_url(host: &str, port: u16, provider: &str) -> String {
    format!("http://{host}:{port}/model/{provider}")
}

/// Wire every configured provider to the gateway with `token` as the
/// placeholder key. The token doubles as the gateway's authentication, so the
/// only secret the harness ever holds is one that names its own session.
pub fn harness_wiring(cfg: &Config, host: &str, token: &str) -> Wiring {
    let mut env = Vec::new();
    let mut providers = serde_json::Map::new();
    for (name, p) in &cfg.providers {
        let base = base_url(host, cfg.gateway.forward_port, name);
        match p.shape.as_str() {
            SHAPE_ANTHROPIC => {
                env.push(("ANTHROPIC_BASE_URL".to_string(), base));
                env.push(("ANTHROPIC_API_KEY".to_string(), token.to_string()));
            }
            _ => {
                providers.insert(
                    name.clone(),
                    json!({ "baseUrl": format!("{base}/v1"), "apiKey": token }),
                );
            }
        }
    }
    let models_json = serde_json::to_string_pretty(&json!({ "providers": providers }))
        .unwrap_or_else(|_| "{}".into());
    Wiring { env, models_json }
}

fn refuse(status: StatusCode, reason: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": { "type": "tracon_refused", "message": reason } })),
    )
        .into_response()
}

/// Who is calling: a session (with its channel) or the node itself, probing
/// the model catalogue or embedding its own corpus.
enum Caller {
    Session { id: String, channel: String },
    Probe,
}

/// Whether a proxied path is an embeddings call. Matched on the whole final
/// segment rather than a prefix, so `/v1/embeddings-and-more` is not one.
fn is_embeddings(rest: &str) -> bool {
    rest.trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(|last| last == "embeddings")
}

/// `ANY /model/{provider}/{*rest}`.
pub async fn handle(
    State(s): State<AppState>,
    Path((provider, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let presented = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string)
        })
        .unwrap_or_default();
    let caller = if s.manager.is_probe_token(&presented) {
        Caller::Probe
    } else if let Some((id, channel)) = s.manager.session_for_token(&presented).await {
        Caller::Session { id, channel }
    } else {
        return refuse(StatusCode::UNAUTHORIZED, "unauthorized");
    };
    // The node's own token reads the model catalogue and writes exactly one
    // thing: an embedding. Widening it to POST at all is narrow on purpose —
    // an embeddings path and nothing else — because this token is not scoped
    // to a channel, so anything it can reach is outside the per-channel
    // provider bindings that every session call is held to.
    if matches!(caller, Caller::Probe) && method != Method::GET && !is_embeddings(&rest) {
        return refuse(
            StatusCode::FORBIDDEN,
            "the node's own token may only read, or embed",
        );
    }

    let Some(p) = s.cfg.providers.get(&provider).cloned() else {
        return refuse(
            StatusCode::NOT_FOUND,
            &format!("no provider named {provider} on this node"),
        );
    };
    // The upstream must also pass the egress allowlist: the gateway cannot be
    // a wider hole than CONNECT was.
    let upstream_host = reqwest::Url::parse(&p.upstream)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let allowed = super::proxy::Allowlist::new(&s.cfg.gateway.allow_hosts)
        .map(|a| a.allows(&upstream_host))
        .unwrap_or(false);
    if !allowed {
        return refuse(
            StatusCode::FORBIDDEN,
            &format!("upstream {upstream_host} is not on the egress allowlist"),
        );
    }

    let (session_id, channel) = match &caller {
        Caller::Session { id, channel } => (Some(id.clone()), Some(channel.clone())),
        Caller::Probe => (None, None),
    };
    // The ceiling, enforced where the spending happens. The harness sees the
    // error and the turn fails; the session stays for the operator to decide.
    if let (Some(sid), Some(ch)) = (&session_id, &channel) {
        let bindings = s.manager.bindings(ch);
        let ceiling = crate::metrics::ceiling(s.manager.store(), &bindings, ch);
        if ceiling.at() {
            if !s
                .manager
                .store()
                .has_event(sid, ek::CEILING)
                .unwrap_or(true)
            {
                s.manager.record_event(
                    sid,
                    ek::CEILING,
                    json!({ "channel": ch, "usage_today": ceiling.usage_today, "ceiling": ceiling.ceiling }),
                );
            }
            return refuse(
                StatusCode::TOO_MANY_REQUESTS,
                &format!("channel {ch} is at its daily ceiling: {}", ceiling.reason()),
            );
        }
    }
    let injection = match decide(&s, &provider, &p, channel.as_deref()) {
        Ok(i) => i,
        Err(reason) => {
            tracing::warn!(provider, channel = ?channel, %reason, "model request refused");
            return refuse(StatusCode::FORBIDDEN, &reason);
        }
    };

    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let target = format!("{}/{}{}", p.upstream.trim_end_matches('/'), rest, query);
    let mut req = s.tools.http.request(method.clone(), &target);
    for (k, v) in headers.iter() {
        if matches!(
            k.as_str(),
            "host" | "authorization" | "x-api-key" | "content-length" | "connection"
        ) || (injection.oauth_beta && k == "anthropic-beta")
        {
            continue;
        }
        req = req.header(k, v);
    }
    req = injection.apply(req, &headers);
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v["model"].as_str().map(str::to_string));
    req = req.body(body);

    let upstream = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(provider, error = %e, "upstream unreachable");
            return refuse(StatusCode::BAD_GATEWAY, "upstream unreachable");
        }
    };
    let status = upstream.status();
    let mut out = Response::builder().status(status.as_u16());
    for (k, v) in upstream.headers() {
        if matches!(
            k.as_str(),
            "transfer-encoding" | "content-length" | "connection"
        ) {
            continue;
        }
        out = out.header(k, v);
    }
    let usage = Arc::new(Mutex::new(UsageRow {
        channel: channel.clone().unwrap_or_default(),
        node_id: s.node_id.clone(),
        session_id,
        provider: provider.clone(),
        model,
        at_ms: now_ms(),
        input_tokens: 0,
        output_tokens: 0,
        requests: 1,
    }));
    let counted = Counted {
        inner: Box::pin(upstream.bytes_stream()),
        scanner: UsageScanner::new(&p.shape),
        usage: usage.clone(),
        store: s.manager.store().clone(),
        done: false,
    };
    out.body(Body::from_stream(counted))
        .unwrap_or_else(|_| refuse(StatusCode::BAD_GATEWAY, "bad upstream response"))
}

/// Provider bindings, then the credential's own bindings. Fail closed: a
/// channel bound to a provider list is refused anything off it, and a probe
/// gets only a credential some channel on this node could use.
fn decide(
    s: &AppState,
    provider: &str,
    p: &Provider,
    channel: Option<&str>,
) -> Result<Injection, String> {
    if let Some(ch) = channel {
        if let Ok(Some(row)) = s.manager.store().channel_get(ch) {
            let bindings: Value = serde_json::from_str(&row.bindings_json).unwrap_or(Value::Null);
            if let Some(list) = bindings["providers"].as_array() {
                if !list.iter().any(|v| v.as_str() == Some(provider)) {
                    return Err(format!("channel {ch} is not bound to provider {provider}"));
                }
            }
        }
    }
    let broker = s.tools.broker.read().unwrap();
    match channel {
        Some(ch) => broker
            .inject_for(&p.credential, ch, &s.node_id, &p.shape)
            .map_err(|e| e.to_string()),
        None => broker
            .inject_for_probe(&p.credential, &s.node_id, &p.shape)
            .map_err(|e| e.to_string()),
    }
}

impl Injection {
    fn apply(
        &self,
        mut req: reqwest::RequestBuilder,
        incoming: &HeaderMap,
    ) -> reqwest::RequestBuilder {
        if let Some(a) = &self.authorization {
            req = req.header("authorization", a);
        }
        if let Some(k) = &self.x_api_key {
            req = req.header("x-api-key", k);
        }
        if self.oauth_beta {
            // Merge rather than replace: the harness's own beta flags are part
            // of the request shape the token was issued for.
            let mut flags: Vec<String> = incoming
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(',').map(|f| f.trim().to_string()).collect())
                .unwrap_or_default();
            if !flags.iter().any(|f| f == ANTHROPIC_OAUTH_BETA) {
                flags.insert(0, ANTHROPIC_OAUTH_BETA.into());
            }
            if let Ok(v) = HeaderValue::from_str(&flags.join(",")) {
                req = req.header("anthropic-beta", v);
            }
        }
        req
    }
}

/// Pulls token counts out of the response as it streams by, from the `usage`
/// objects both shapes emit; the body itself passes through untouched.
struct UsageScanner {
    shape: String,
    line: Vec<u8>,
    input: i64,
    output: i64,
}

impl UsageScanner {
    fn new(shape: &str) -> Self {
        Self {
            shape: shape.to_string(),
            line: Vec::new(),
            input: 0,
            output: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.scan_line(&line);
            } else if self.line.len() < 1 << 20 {
                self.line.push(b);
            }
        }
    }

    fn finish(&mut self) {
        let line = std::mem::take(&mut self.line);
        self.scan_line(&line);
    }

    fn scan_line(&mut self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        let json = text.strip_prefix("data:").map(str::trim).unwrap_or(text);
        if !json.contains("usage") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(json) else {
            return;
        };
        let usage = if v["usage"].is_object() {
            &v["usage"]
        } else if v["message"]["usage"].is_object() {
            &v["message"]["usage"]
        } else if v["response"]["usage"].is_object() {
            &v["response"]["usage"]
        } else {
            return;
        };
        let (i, o) = if self.shape == SHAPE_OPENAI {
            (
                usage["input_tokens"]
                    .as_i64()
                    .or_else(|| usage["prompt_tokens"].as_i64()),
                usage["output_tokens"]
                    .as_i64()
                    .or_else(|| usage["completion_tokens"].as_i64()),
            )
        } else {
            (
                usage["input_tokens"].as_i64(),
                usage["output_tokens"].as_i64(),
            )
        };
        // Cumulative in a stream (Anthropic's message_delta repeats the running
        // output count), so keep the largest seen.
        if let Some(i) = i {
            self.input = self.input.max(i);
        }
        if let Some(o) = o {
            self.output = self.output.max(o);
        }
    }
}

struct Counted {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    scanner: UsageScanner,
    usage: Arc<Mutex<UsageRow>>,
    store: Arc<crate::store::Store>,
    done: bool,
}

impl Counted {
    fn record(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.scanner.finish();
        let mut row = self.usage.lock().unwrap().clone();
        row.input_tokens = self.scanner.input;
        row.output_tokens = self.scanner.output;
        if let Err(e) = self.store.record_usage(&row) {
            tracing::warn!(error = %e, "usage not recorded");
        }
    }
}

impl Stream for Counted {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => {
                self.scanner.feed(&b);
                Poll::Ready(Some(Ok(b)))
            }
            Poll::Ready(Some(Err(e))) => {
                self.record();
                Poll::Ready(Some(Err(std::io::Error::other(e))))
            }
            Poll::Ready(None) => {
                self.record();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        // A client that hangs up mid-stream still made the request.
        self.record();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiring_puts_anthropic_in_env_and_openai_in_models_json() {
        let cfg = Config::default();
        let w = harness_wiring(&cfg, "tracon-gw", "tok");
        assert!(w.env.contains(&(
            "ANTHROPIC_BASE_URL".into(),
            "http://tracon-gw:7421/model/anthropic".into()
        )));
        assert!(w.env.contains(&("ANTHROPIC_API_KEY".into(), "tok".into())));
        let v: Value = serde_json::from_str(&w.models_json).unwrap();
        assert_eq!(
            v["providers"]["openai"]["baseUrl"],
            "http://tracon-gw:7421/model/openai/v1"
        );
        assert_eq!(v["providers"]["openai"]["apiKey"], "tok");
        assert!(v["providers"]["anthropic"].is_null());
    }

    #[test]
    fn the_scanner_reads_both_shapes_and_keeps_the_running_maximum() {
        let mut s = UsageScanner::new(SHAPE_ANTHROPIC);
        s.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\n\n");
        s.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n");
        s.finish();
        assert_eq!((s.input, s.output), (7, 4));

        let mut s = UsageScanner::new(SHAPE_OPENAI);
        s.feed(b"{\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}");
        s.finish();
        assert_eq!((s.input, s.output), (10, 3));
        let mut s = UsageScanner::new(SHAPE_OPENAI);
        s.feed(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":5}}}\n");
        s.finish();
        assert_eq!((s.input, s.output), (12, 5));
    }
}
