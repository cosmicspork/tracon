//! The model gateway: the harness sends its ordinary provider request to
//! `http://tracon-gw:<forward>/model/<provider>/…` carrying a placeholder key
//! (its session token), and the node swaps in the real credential and forwards
//! over TLS. No interception — the request is the harness's own, so the shape
//! a subscription token demands is preserved. This is the enforcement point for
//! provider bindings and the counting point for usage: every model call passes
//! through here, so cost is measured where it happens rather than reported by
//! the harness.
//!
//! The credential is lent for inference and nothing else. The harness holds a
//! session token rather than the key, so the only provider-account operations
//! it can reach are the ones this gateway agrees to forward: an explicit
//! per-shape allowlist of (method, path), matched on a normalised path, with
//! everything else refused before the credential is attached.

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
    config::{Config, Provider, SHAPE_ANTHROPIC, SHAPE_OPENAI_CODEX},
    http::api::AppState,
    store::{now_ms, UsageRow},
};

/// The beta flag Anthropic's subscription tokens are issued under. Verified
/// against a live token on 2026-09-12: necessary but not sufficient — see
/// `CLAUDE_CODE_SYSTEM`.
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// A subscription token is only honoured for requests whose system prompt
/// opens with this sentence; anything else is answered `429
/// rate_limit_error` with the message `Error`, indistinguishable from a real
/// limit. Observed on 2026-09-12 against a live token through this gateway:
/// the same token, same model, same message, 200 with the sentence, 429
/// without it; the user-agent made no difference. A harness that knows it
/// holds an OAuth token prepends this itself; through the gateway the harness
/// believes it holds an API key, so the gateway does it.
const CLAUDE_CODE_SYSTEM: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Rewrite an Anthropic Messages body so its system prompt opens with the
/// sentence a subscription token demands. `None` when the body is not a JSON
/// object, has no `messages`, or already opens with it — the body then goes
/// through untouched.
fn shaped_for_subscription(body: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object_mut()?;
    if !object.contains_key("messages") {
        return None;
    }
    let prefix = json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM });
    let system = match object.remove("system") {
        None | Some(Value::Null) => vec![prefix],
        Some(Value::String(text)) => {
            if text.starts_with(CLAUDE_CODE_SYSTEM) {
                return None;
            }
            vec![prefix, json!({ "type": "text", "text": text })]
        }
        Some(Value::Array(blocks)) => {
            if blocks
                .first()
                .and_then(|block| block["text"].as_str())
                .is_some_and(|text| text.starts_with(CLAUDE_CODE_SYSTEM))
            {
                return None;
            }
            std::iter::once(prefix).chain(blocks).collect()
        }
        Some(other) => vec![prefix, other],
    };
    object.insert("system".into(), Value::Array(system));
    serde_json::to_vec(&value).ok()
}

/// One provider as this session may reach it: where the gateway serves it and
/// which models this node declares under it.
///
/// A harness that reads no base-URL environment variable (OpenCode; see
/// `docs/reference/opencode-v1.18.30/providers.md` §2.5) cannot be wired by
/// `env` at all, and one that declares its catalogue rather than probing it
/// needs the model list before it starts. Both come from here, so neither is
/// reconstructed from `models_json` by string surgery.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderWiring {
    /// The provider's name in this node's configuration, which is also the
    /// name the gateway routes by and the harness's provider id.
    pub name: String,
    /// `anthropic`, `openai`, `openai-codex`: which request shape it is.
    pub shape: String,
    /// The gateway base URL for it, without a version suffix.
    pub base_url: String,
    /// The models this node declares under it. Empty means none were declared
    /// — a harness that probes its own catalogue is unaffected.
    pub models: Vec<crate::config::ModelDecl>,
}

/// What a harness needs to reach the gateway: environment for the providers
/// that honour one, a `models.json` for the ones that only read a provider
/// override (omp's `openai`), and the same wiring in structured form for the
/// ones whose entire provider table is a file this node writes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wiring {
    pub env: Vec<(String, String)>,
    pub models_json: String,
    pub providers: Vec<ProviderWiring>,
    /// The placeholder key every provider entry carries, which is also this
    /// session's token at the gateway.
    pub token: String,
}

/// The base URL for one provider as the harness sees it.
pub fn base_url(host: &str, port: u16, provider: &str) -> String {
    format!("http://{host}:{port}/model/{provider}")
}

/// Wire the providers `servable` accepts to the gateway with `token` as the
/// placeholder key. The token doubles as the gateway's authentication, so the
/// only secret the harness ever holds is one that names its own session.
///
/// `servable` answers the question the gateway answers again at request time:
/// would this caller's model request for that provider be allowed through?
/// Wiring a provider the gateway would refuse is not merely useless — the
/// harness reads this file as its provider list and offers the whole
/// catalogue of everything in it, so an unusable `openai` entry buries the
/// Codex models the node can actually run under fifty-odd OpenAI API ones,
/// several of which are Codex models by name (`openai/gpt-5.3-codex`). A
/// session started on one of those is refused at the gateway, having never
/// been reachable.
pub fn harness_wiring(
    cfg: &Config,
    host: &str,
    token: &str,
    servable: impl Fn(&str, &Provider) -> bool,
) -> Wiring {
    let mut env = Vec::new();
    let mut providers = serde_json::Map::new();
    let mut wired = Vec::new();
    for (name, provider) in &cfg.providers {
        if !servable(name, provider) {
            continue;
        }
        let base = base_url(host, cfg.gateway.forward_port, name);
        wired.push(ProviderWiring {
            name: name.clone(),
            shape: provider.shape.clone(),
            base_url: base.clone(),
            models: provider.models.clone(),
        });
        match provider.shape.as_str() {
            SHAPE_ANTHROPIC => {
                env.push(("ANTHROPIC_BASE_URL".to_string(), base));
                env.push(("ANTHROPIC_API_KEY".to_string(), token.to_string()));
            }
            SHAPE_OPENAI_CODEX => {
                env.push(("PI_CODEX_WEBSOCKET".to_string(), "false".to_string()));
                providers.insert(name.clone(), json!({ "baseUrl": base, "apiKey": token }));
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
    Wiring {
        env,
        models_json,
        providers: wired,
        token: token.to_string(),
    }
}

/// Models a Codex provider offers that a ChatGPT subscription cannot run: the
/// account answers `not supported when using Codex with a ChatGPT account`.
/// The harness image has no egress but the gateway, so it cannot refresh its
/// model catalogue and offers the one its pinned build shipped with; the
/// catalogue upstream publishes today lists neither of these under
/// `openai-codex`. Nothing in the harness's list says which entries an
/// account kind may use, so this is a denylist keyed on the known refusal
/// rather than something derived.
const CHATGPT_ACCOUNT_REFUSES: &[&str] = &["gpt-5.4", "gpt-5.4-mini"];

/// The probed catalogue as this node can honestly offer it. `subscription`
/// says whether the credential bound to a provider is an OAuth subscription
/// rather than an API key; only a subscription is held to the list above, and
/// only for a Codex-shaped provider. Every other model the harness reported
/// survives, including ones this build has never heard of.
pub fn offerable(
    cfg: &Config,
    models: Vec<crate::adapter::ModelOption>,
    subscription: impl Fn(&str) -> bool,
) -> Vec<crate::adapter::ModelOption> {
    models
        .into_iter()
        .filter(|option| {
            let Some((provider_name, model)) = option.value.split_once('/') else {
                return true;
            };
            let Some(provider) = cfg.providers.get(provider_name) else {
                return true;
            };
            provider.shape != SHAPE_OPENAI_CODEX
                || !CHATGPT_ACCOUNT_REFUSES.contains(&model)
                || !subscription(provider_name)
        })
        .collect()
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

/// One call the gateway forwards: a method, and the path as segments, where
/// `*` stands for one opaque segment (a model id, a response id).
type Route = (&'static str, &'static [&'static str]);

/// The inference surface of an Anthropic-shaped provider: what the Claude and
/// omp adapters send at `ANTHROPIC_BASE_URL`. Everything else on
/// `api.anthropic.com` — the organization, workspace, invite, API-key and
/// usage-report endpoints a Console key can drive — is refused here, because
/// a session was granted a model, not the account behind it.
const ANTHROPIC_ROUTES: &[Route] = &[
    ("POST", &["v1", "messages"]),
    ("POST", &["v1", "messages", "count_tokens"]),
    ("GET", &["v1", "models"]),
    ("GET", &["v1", "models", "*"]),
];

/// The inference surface of an OpenAI-shaped provider. The harness is wired to
/// `…/model/<name>/v1`, so every path it sends opens with `v1`. Files,
/// fine-tuning, batches, assistants, containers and everything under
/// `/v1/organization` are off it deliberately: each one either spends money
/// outside the metered path or reads the account.
const OPENAI_ROUTES: &[Route] = &[
    ("POST", &["v1", "chat", "completions"]),
    ("POST", &["v1", "responses"]),
    ("GET", &["v1", "responses", "*"]),
    ("POST", &["v1", "embeddings"]),
    ("GET", &["v1", "models"]),
    ("GET", &["v1", "models", "*"]),
];

/// The inference surface of a Codex-shaped provider, whose upstream is the
/// ChatGPT backend rather than the API: the Responses call the Codex client
/// makes, under the prefixes its builds have used, and the catalogue. The rest
/// of `chatgpt.com/backend-api` is the subscriber's account — conversations,
/// settings, billing — and is refused.
const OPENAI_CODEX_ROUTES: &[Route] = &[
    ("POST", &["responses"]),
    ("POST", &["codex", "responses"]),
    ("POST", &["v1", "responses"]),
    ("GET", &["responses", "*"]),
    ("GET", &["codex", "responses", "*"]),
    ("GET", &["models"]),
    ("GET", &["codex", "models"]),
    ("GET", &["v1", "models"]),
];

/// The allowlist for a shape. A shape this build does not know is held to the
/// OpenAI surface rather than waved through: an unknown shape is wired as an
/// OpenAI-compatible one (see `harness_wiring`), and fail-closed is the point.
fn routes(shape: &str) -> &'static [Route] {
    match shape {
        SHAPE_ANTHROPIC => ANTHROPIC_ROUTES,
        SHAPE_OPENAI_CODEX => OPENAI_CODEX_ROUTES,
        _ => OPENAI_ROUTES,
    }
}

/// A segment standing in for a model or response id: printable ASCII from a
/// narrow set, so a matched path never needs re-encoding to be forwarded.
pub(crate) fn is_opaque(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 128
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        && segment != "."
        && segment != ".."
}

/// Percent-decode one path segment, once. `None` for a truncated or non-hex
/// escape, or bytes that are not UTF-8.
fn decode_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (hi, lo) = (*bytes.get(i + 1)?, *bytes.get(i + 2)?);
            if !hi.is_ascii_hexdigit() || !lo.is_ascii_hexdigit() {
                return None;
            }
            out.push(u8::from_str_radix(&segment[i + 1..i + 3], 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The path to match and forward, from the raw tail of the request path: each
/// segment decoded once, empty and `.` segments dropped, and anything still
/// ambiguous after decoding — `..`, an encoded separator, a second layer of
/// encoding — refused rather than resolved. Matching the decoded form and
/// forwarding that same form is what keeps the two from disagreeing.
pub(crate) fn normalised(raw: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for segment in raw.split('/') {
        let decoded = decode_segment(segment)?;
        match decoded.as_str() {
            "" | "." => continue,
            ".." => return None,
            _ => {}
        }
        if decoded
            .bytes()
            .any(|b| matches!(b, b'/' | b'\\' | b'?' | b'#' | b'%' | 0..=0x20 | 0x7f))
        {
            return None;
        }
        out.push(decoded);
    }
    (!out.is_empty()).then_some(out)
}

/// The node's own token reads the catalogue and writes exactly one thing: an
/// embedding. It is not scoped to a channel, so anything it reaches is outside
/// the per-channel provider bindings every session call is held to — it gets
/// the models list and the embeddings call, never inference.
fn probe_may(method: &Method, path: &[String]) -> bool {
    let last = path.last().map(String::as_str);
    (method == Method::GET && last == Some("models"))
        || (method == Method::POST && last == Some("embeddings"))
}

/// The part of `/model/<provider>/<tail>` the provider is being asked for,
/// still percent-encoded as it arrived.
fn raw_tail(path: &str) -> Option<&str> {
    path.strip_prefix("/model/")?
        .split_once('/')
        .map(|(_, t)| t)
}

/// The normalised path to forward, or why this call is not one the gateway
/// lends the credential to.
fn allowed_call(
    shape: &str,
    probe: bool,
    method: &Method,
    raw_tail: &str,
) -> Result<String, String> {
    let refused = |path: &str| {
        format!("{method} /{path} is not a model call this gateway forwards; the credential is lent for inference only")
    };
    let Some(path) = normalised(raw_tail) else {
        let shown: String = raw_tail.chars().take(120).collect();
        return Err(refused(&shown));
    };
    let allowed = routes(shape).iter().any(|(verb, want)| {
        method.as_str() == *verb
            && want.len() == path.len()
            && want.iter().zip(&path).all(|(want, got)| {
                if *want == "*" {
                    is_opaque(got)
                } else {
                    want == got
                }
            })
    });
    let joined = path.join("/");
    if !allowed {
        return Err(refused(&joined));
    }
    if probe && !probe_may(method, &path) {
        return Err(format!(
            "{method} /{joined} is not a call the node's own token may make; it may list models, or embed"
        ));
    }
    Ok(joined)
}

/// `ANY /model/{provider}/{*rest}`.
pub async fn handle(
    State(s): State<AppState>,
    Path((provider, _rest)): Path<(String, String)>,
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
    let Some(p) = s.cfg.providers.get(&provider).cloned() else {
        return refuse(
            StatusCode::NOT_FOUND,
            &format!("no provider named {provider} on this node"),
        );
    };

    let (session_id, channel) = match &caller {
        Caller::Session { id, channel } => (Some(id.clone()), Some(channel.clone())),
        Caller::Probe => (None, None),
    };
    // What the credential is lent for. Matched on the raw path rather than
    // axum's decoded capture, which has already lost the difference between a
    // separator the caller sent and one it encoded.
    let Some(raw_tail) = raw_tail(uri.path()) else {
        return refuse(StatusCode::NOT_FOUND, "not a model path");
    };
    let rest = match allowed_call(&p.shape, matches!(caller, Caller::Probe), &method, raw_tail) {
        Ok(path) => path,
        Err(reason) => {
            tracing::warn!(provider, channel = ?channel, method = %method, %reason, "model call refused: off the gateway's allowlist");
            if let Some(session_id) = &session_id {
                note_refusal(&s, session_id, &provider, &method, &reason);
            }
            return refuse(StatusCode::FORBIDDEN, &reason);
        }
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
        ) || (p.shape == SHAPE_OPENAI_CODEX && k == "chatgpt-account-id")
            || (injection.oauth_beta && k == "anthropic-beta")
        {
            continue;
        }
        req = req.header(k, v);
    }
    req = injection.apply(req, &headers);
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v["model"].as_str().map(str::to_string));
    let body = if injection.oauth_beta && p.shape == SHAPE_ANTHROPIC {
        shaped_for_subscription(&body)
            .map(Bytes::from)
            .unwrap_or(body)
    } else {
        body
    };
    req = req.body(body);

    if let Some(session_id) = &session_id {
        if let Err(error) = s.manager.ensure_active(session_id) {
            return refuse(StatusCode::CONFLICT, &error.to_string());
        }
    }
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
        session_id: session_id.clone(),
        provider: provider.clone(),
        model,
        at_ms: now_ms(),
        input_tokens: 0,
        output_tokens: 0,
        requests: 1,
    }));
    // A refused call is the harness's business — it retries inside the turn,
    // with backoff, and says nothing over ACP until it gives up, so the
    // session reads as a hang. The gateway is the one place that sees the
    // upstream answer, so it tells the session instead. The body is buffered
    // rather than streamed only on this path: an error body is small, and it
    // still reaches the harness byte for byte.
    let inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
        if status.is_client_error() || status.is_server_error() {
            let body = upstream.bytes().await.unwrap_or_else(|e| {
                tracing::warn!(provider, error = %e, "error body from upstream was cut short");
                Bytes::new()
            });
            if let Some(session_id) = &session_id {
                note_provider_error(&s, session_id, &provider, status, &body);
            }
            Box::pin(tokio_stream::once(Ok(body)))
        } else {
            Box::pin(upstream.bytes_stream())
        };
    let counted = Counted {
        inner,
        scanner: UsageScanner::new(),
        usage: usage.clone(),
        store: s.manager.store().clone(),
        expect_usage: status.is_success(),
        state: s.clone(),
        done: false,
    };
    out.body(Body::from_stream(counted))
        .unwrap_or_else(|_| refuse(StatusCode::BAD_GATEWAY, "bad upstream response"))
}

/// Tell the session — and so the operator reading its log — that a call was
/// refused before it reached the provider. Rate-limited like `provider_error`,
/// since a harness that keeps trying would otherwise fill the transcript.
fn note_refusal(s: &AppState, session_id: &str, provider: &str, method: &Method, reason: &str) {
    let attempt = s
        .manager
        .store()
        .count_events_this_turn(session_id, ek::GATEWAY_REFUSED)
        .unwrap_or(0)
        + 1;
    if attempt > MAX_PROVIDER_ERRORS_PER_TURN {
        return;
    }
    s.manager.record_event(
        session_id,
        ek::GATEWAY_REFUSED,
        json!({
            "provider": provider,
            "method": method.as_str(),
            "reason": reason,
            "attempt": attempt,
        }),
    );
}

/// A provider answered without reporting what the call cost. The gateway
/// counts on the wire, so what the upstream omits it cannot count — and the
/// harness cannot either: OpenCode maps a missing `usage` to zero with no
/// estimator anywhere in its accounting path (`providers.md` §6.3). Zero
/// tokens and unknown tokens are different facts, and a budget stated against
/// the first while the second is true is the failure that finding is about.
/// So the session says so, once, rather than reading as a free model.
fn note_unmetered(s: &AppState, row: &UsageRow) {
    let Some(session_id) = &row.session_id else {
        return;
    };
    if s.manager
        .store()
        .has_event(session_id, ek::UNMETERED)
        .unwrap_or(true)
    {
        return;
    }
    tracing::warn!(
        provider = %row.provider,
        "provider reported no usage: this session is unmetered on it, not free"
    );
    s.manager.record_event(
        session_id,
        ek::UNMETERED,
        json!({ "provider": row.provider, "model": row.model }),
    );
}

/// At most this many `provider_error` events per turn. A harness that retries
/// without ever giving up would otherwise write the log full; the operator
/// learns nothing from attempt 40 that attempt 20 did not already say.
const MAX_PROVIDER_ERRORS_PER_TURN: i64 = 20;

/// Record one failed upstream attempt on the session that made the call. The
/// session state is untouched: the harness has not given up, and this is
/// informational.
fn note_provider_error(
    s: &AppState,
    session_id: &str,
    provider: &str,
    status: StatusCode,
    body: &[u8],
) {
    let attempt = s
        .manager
        .store()
        .count_events_this_turn(session_id, ek::PROVIDER_ERROR)
        .unwrap_or(0)
        + 1;
    let message = short_error(status, body);
    tracing::warn!(provider, status = status.as_u16(), attempt, %message, "provider refused a session's model call");
    if attempt > MAX_PROVIDER_ERRORS_PER_TURN {
        return;
    }
    s.manager.record_event(
        session_id,
        ek::PROVIDER_ERROR,
        json!({
            "provider": provider,
            "status": status.as_u16(),
            "message": message,
            "attempt": attempt,
        }),
    );
}

/// One short line for the transcript out of a provider's error body. Both
/// shapes nest the human-readable part differently, and a body that is not
/// JSON at all (an edge proxy's HTML) still has to become something readable.
fn short_error(status: StatusCode, body: &[u8]) -> String {
    let value: Option<Value> = serde_json::from_slice(body).ok();
    let text = value
        .as_ref()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .or_else(|| v["error"].as_str())
        })
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(body).to_string());
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = line.chars().take(200).collect();
    if line.is_empty() {
        return status
            .canonical_reason()
            .unwrap_or("upstream error")
            .to_string();
    }
    line
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
        if let Some(account_id) = &self.chatgpt_account_id {
            req = req.header("chatgpt-account-id", account_id);
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
/// objects every shape emits; the body itself passes through untouched.
///
/// Both spellings are read for every shape rather than one per shape. The
/// shape names the *request* surface, not the dialect of the answer: an
/// OpenAI-compatible server reached through a provider whose shape this build
/// does not recognise still answers `prompt_tokens`/`completion_tokens`, and a
/// scanner that only looked for `input_tokens` would count it as zero — which
/// is the failure `docs/reference/opencode-v1.18.30/providers.md` §6.3 names,
/// arrived at from the gateway's side instead of the harness's.
struct UsageScanner {
    line: Vec<u8>,
    input: i64,
    output: i64,
    /// Whether any `usage` the scanner understood went by. A response that
    /// carried none is unmetered, which is not the same fact as zero tokens.
    metered: bool,
}

impl UsageScanner {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            input: 0,
            output: 0,
            metered: false,
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
        let i = usage["input_tokens"]
            .as_i64()
            .or_else(|| usage["prompt_tokens"].as_i64());
        let o = usage["output_tokens"]
            .as_i64()
            .or_else(|| usage["completion_tokens"].as_i64());
        // Cumulative in a stream (Anthropic's message_delta repeats the running
        // output count), so keep the largest seen.
        if let Some(i) = i {
            self.metered = true;
            self.input = self.input.max(i);
        }
        if let Some(o) = o {
            self.metered = true;
            self.output = self.output.max(o);
        }
    }
}

struct Counted {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    scanner: UsageScanner,
    usage: Arc<Mutex<UsageRow>>,
    store: Arc<crate::store::Store>,
    /// The node, so a provider that reported nothing can be marked on the
    /// session that called it rather than silently recorded as free.
    state: AppState,
    /// Whether an answer was expected to carry usage at all. An upstream error
    /// carries none and is not evidence that the provider is unmetered.
    expect_usage: bool,
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
        if self.expect_usage && !self.scanner.metered {
            note_unmetered(&self.state, &row);
        }
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
    use crate::config::SHAPE_OPENAI;

    fn system_of(body: &[u8]) -> Value {
        serde_json::from_slice::<Value>(body).unwrap()["system"].clone()
    }

    #[test]
    fn a_subscription_request_opens_with_the_sentence_the_token_demands() {
        let plain = br#"{"model":"m","messages":[],"system":"Be terse."}"#;
        let shaped = shaped_for_subscription(plain).expect("rewritten");
        let system = system_of(&shaped);
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM);
        assert_eq!(system[1]["text"], "Be terse.");

        let blocks =
            br#"{"model":"m","messages":[],"system":[{"type":"text","text":"Be terse."}]}"#;
        let system = system_of(&shaped_for_subscription(blocks).expect("rewritten"));
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM);
        assert_eq!(system[1]["text"], "Be terse.");

        let none = br#"{"model":"m","messages":[]}"#;
        let system = system_of(&shaped_for_subscription(none).expect("rewritten"));
        assert_eq!(system.as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn a_request_that_already_opens_with_it_or_is_not_a_messages_call_passes_untouched() {
        let already =
            format!(r#"{{"model":"m","messages":[],"system":"{CLAUDE_CODE_SYSTEM} More."}}"#);
        assert!(shaped_for_subscription(already.as_bytes()).is_none());
        let already_blocks = format!(
            r#"{{"model":"m","messages":[],"system":[{{"type":"text","text":"{CLAUDE_CODE_SYSTEM}"}}]}}"#
        );
        assert!(shaped_for_subscription(already_blocks.as_bytes()).is_none());
        assert!(shaped_for_subscription(br#"{"model":"m"}"#).is_none());
        assert!(shaped_for_subscription(b"not json").is_none());
    }

    #[test]
    fn an_error_body_becomes_one_readable_line() {
        let anthropic =
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;
        assert_eq!(
            short_error(StatusCode::TOO_MANY_REQUESTS, anthropic),
            "Error"
        );
        let openai = br#"{"message":"You exceeded your\n   current quota"}"#;
        assert_eq!(
            short_error(StatusCode::TOO_MANY_REQUESTS, openai),
            "You exceeded your current quota"
        );
        // An edge that answers HTML, and one that answers nothing at all.
        assert_eq!(
            short_error(
                StatusCode::BAD_GATEWAY,
                b"<html>\n<body>502 Bad Gateway</body>"
            ),
            "<html> <body>502 Bad Gateway</body>"
        );
        assert_eq!(short_error(StatusCode::BAD_GATEWAY, b""), "Bad Gateway");
        // Long enough to fill the transcript, cut to a line.
        assert_eq!(
            short_error(StatusCode::BAD_REQUEST, &vec![b'x'; 4096]).len(),
            200
        );
    }

    fn allowed(shape: &str, method: &str, tail: &str) -> Result<String, String> {
        allowed_call(
            shape,
            false,
            &Method::from_bytes(method.as_bytes()).unwrap(),
            tail,
        )
    }

    #[test]
    fn only_the_inference_surface_of_each_shape_is_forwarded() {
        assert_eq!(
            allowed(SHAPE_ANTHROPIC, "POST", "v1/messages"),
            Ok("v1/messages".into())
        );
        assert_eq!(
            allowed(SHAPE_ANTHROPIC, "POST", "v1/messages/count_tokens"),
            Ok("v1/messages/count_tokens".into())
        );
        assert_eq!(
            allowed(SHAPE_ANTHROPIC, "GET", "v1/models/claude-opus-5"),
            Ok("v1/models/claude-opus-5".into())
        );
        assert_eq!(
            allowed(SHAPE_OPENAI, "POST", "v1/chat/completions"),
            Ok("v1/chat/completions".into())
        );
        assert_eq!(
            allowed(SHAPE_OPENAI, "POST", "v1/embeddings"),
            Ok("v1/embeddings".into())
        );
        assert_eq!(
            allowed(SHAPE_OPENAI_CODEX, "POST", "codex/responses"),
            Ok("codex/responses".into())
        );

        // The account behind the credential, by every route to it.
        for (shape, method, tail) in [
            (SHAPE_ANTHROPIC, "GET", "v1/organizations/me/api_keys"),
            (SHAPE_ANTHROPIC, "POST", "v1/organizations/invites"),
            (
                SHAPE_ANTHROPIC,
                "GET",
                "v1/organizations/usage_report/messages",
            ),
            (SHAPE_ANTHROPIC, "DELETE", "v1/messages"),
            (SHAPE_ANTHROPIC, "POST", "v1/files"),
            (SHAPE_OPENAI, "GET", "v1/organization/costs"),
            (SHAPE_OPENAI, "POST", "v1/organization/admin_api_keys"),
            (SHAPE_OPENAI, "POST", "v1/files"),
            (SHAPE_OPENAI, "POST", "v1/fine_tuning/jobs"),
            (SHAPE_OPENAI, "POST", "v1/messages"),
            (SHAPE_OPENAI_CODEX, "GET", "accounts/check"),
            (SHAPE_OPENAI_CODEX, "POST", "payments/checkout"),
        ] {
            assert!(
                allowed(shape, method, tail).is_err(),
                "{shape} {method} /{tail} was forwarded"
            );
        }
        // A shape this build does not know is wired as OpenAI-compatible, and
        // held to that surface rather than waved through.
        assert!(allowed("mystery", "POST", "v1/chat/completions").is_ok());
        assert!(allowed("mystery", "GET", "v1/organization/costs").is_err());
    }

    #[test]
    fn a_path_is_matched_and_forwarded_in_the_same_normalised_form() {
        // Empty and `.` segments collapse, and the collapsed path is what is
        // forwarded, so matching and forwarding cannot disagree.
        assert_eq!(
            allowed(SHAPE_ANTHROPIC, "POST", "v1//./messages/"),
            Ok("v1/messages".into())
        );
        // Traversal, encoded separators and a second layer of encoding are
        // refused rather than resolved.
        for tail in [
            "v1/messages/../../v1/organizations/me/api_keys",
            "v1/..%2forganizations",
            "v1/%2e%2e/organizations/me",
            "v1/messages%2f..%2forganizations",
            "v1/mess%",
            "v1/%zzmessages",
            "v1/organizations%00",
            "v1/%252e%252e/organizations",
        ] {
            assert!(allowed(SHAPE_ANTHROPIC, "GET", tail).is_err(), "{tail}");
            assert!(allowed(SHAPE_ANTHROPIC, "POST", tail).is_err(), "{tail}");
        }
        // An encoded path that decodes to something allowed still is.
        assert_eq!(
            allowed(SHAPE_ANTHROPIC, "POST", "v1/%6d%65ssages"),
            Ok("v1/messages".into())
        );
        // The refusal names what was refused.
        let reason = allowed(SHAPE_OPENAI, "DELETE", "v1/organization/projects/p1").unwrap_err();
        assert!(
            reason.contains("DELETE /v1/organization/projects/p1"),
            "{reason}"
        );
        assert_eq!(raw_tail("/model/openai/v1/messages"), Some("v1/messages"));
        assert_eq!(raw_tail("/model/openai"), None);
    }

    #[test]
    fn the_node_s_own_token_lists_models_and_embeds_and_nothing_else() {
        let probe = |method: &str, tail: &str| {
            allowed_call(
                SHAPE_OPENAI,
                true,
                &Method::from_bytes(method.as_bytes()).unwrap(),
                tail,
            )
        };
        assert!(probe("GET", "v1/models").is_ok());
        assert!(probe("POST", "v1/embeddings").is_ok());
        assert!(probe("POST", "v1/chat/completions").is_err());
        assert!(probe("POST", "v1/responses").is_err());
        assert!(probe("GET", "v1/responses/resp_1").is_err());
    }

    #[test]
    fn wiring_puts_anthropic_in_env_and_openai_in_models_json() {
        let cfg = Config::default();
        let w = harness_wiring(&cfg, "tracon-gw", "tok", |_, _| true);
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
        assert_eq!(
            v["providers"]["openai-codex"]["baseUrl"],
            "http://tracon-gw:7421/model/openai-codex"
        );
        assert_eq!(v["providers"]["openai-codex"]["apiKey"], "tok");
        assert!(w
            .env
            .contains(&("PI_CODEX_WEBSOCKET".into(), "false".into())));
    }

    #[test]
    fn a_provider_the_gateway_would_refuse_is_never_wired() {
        // The harness offers the whole catalogue of every provider it is
        // given, so one with no credential behind it would fill the picker
        // with models that cannot run — including Codex models under the
        // wrong provider.
        let cfg = Config::default();
        let w = harness_wiring(&cfg, "tracon-gw", "tok", |name, _| name == "openai-codex");
        let v: Value = serde_json::from_str(&w.models_json).unwrap();
        assert_eq!(
            v["providers"]["openai-codex"]["baseUrl"],
            "http://tracon-gw:7421/model/openai-codex"
        );
        assert!(v["providers"]["openai"].is_null());
        assert!(!w.env.iter().any(|(key, _)| key.starts_with("ANTHROPIC_")));
        assert!(w
            .env
            .contains(&("PI_CODEX_WEBSOCKET".into(), "false".into())));
    }

    fn option(value: &str) -> crate::adapter::ModelOption {
        crate::adapter::ModelOption {
            value: value.into(),
            name: value.into(),
        }
    }

    #[test]
    fn a_subscription_is_not_offered_the_models_the_account_refuses() {
        let cfg = Config::default();
        let probed = [
            "openai-codex/gpt-5.3-codex-spark",
            "openai-codex/gpt-5.4",
            "openai-codex/gpt-5.4-mini",
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.6-terra",
            "openai-codex/gpt-daybreak-blue-latest",
            "anthropic/claude-opus-5",
            "an-alias",
        ]
        .map(option)
        .to_vec();

        let offered = offerable(&cfg, probed.clone(), |provider| provider == "openai-codex");
        assert_eq!(
            offered.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            [
                "openai-codex/gpt-5.3-codex-spark",
                "openai-codex/gpt-5.5",
                "openai-codex/gpt-5.6-terra",
                // A name this build has never heard of is still the harness's
                // to offer; only the known refusals are dropped.
                "openai-codex/gpt-daybreak-blue-latest",
                "anthropic/claude-opus-5",
                "an-alias",
            ]
        );

        // An API key on the same provider runs them, so nothing is dropped.
        let offered = offerable(&cfg, probed, |_| false);
        assert_eq!(offered.len(), 8);
    }

    #[test]
    fn the_scanner_reads_both_shapes_and_keeps_the_running_maximum() {
        let mut s = UsageScanner::new();
        s.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}\n\n");
        s.feed(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n");
        s.finish();
        assert_eq!((s.input, s.output, s.metered), (7, 4, true));

        let mut s = UsageScanner::new();
        s.feed(b"{\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}");
        s.finish();
        assert_eq!((s.input, s.output, s.metered), (10, 3, true));
        let mut s = UsageScanner::new();
        s.feed(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":5}}}\n");
        s.finish();
        assert_eq!((s.input, s.output, s.metered), (12, 5, true));
    }

    /// A self-hosted OpenAI-compatible server is reached through a provider
    /// whose shape this build has no name for — it is wired as
    /// OpenAI-compatible and held to that surface — and it answers the
    /// chat-completions spelling. Reading the spelling per shape counted that
    /// as zero; reading both counts it.
    #[test]
    fn a_shape_this_build_does_not_know_is_still_counted() {
        let mut s = UsageScanner::new();
        s.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}],\"usage\":null}\n");
        s.feed(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":16,\"total_tokens\":58}}\n");
        s.feed(b"data: [DONE]\n");
        s.finish();
        assert_eq!((s.input, s.output, s.metered), (42, 16, true));
    }

    /// And a provider that reports nothing is unmetered, which is a different
    /// fact from zero tokens: the row of zeroes is what a free model looks
    /// like too, so the scanner keeps the distinction for the gateway to
    /// record on the session.
    #[test]
    fn a_provider_that_reports_nothing_is_unmetered_rather_than_zero() {
        let mut s = UsageScanner::new();
        s.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n");
        s.feed(b"data: [DONE]\n");
        s.finish();
        assert_eq!((s.input, s.output, s.metered), (0, 0, false));
        // A usage object with nothing the scanner understands in it says no
        // more than none at all.
        let mut s = UsageScanner::new();
        s.feed(b"data: {\"usage\":{\"tokens\":7}}\n");
        s.finish();
        assert!(!s.metered);
    }
}
