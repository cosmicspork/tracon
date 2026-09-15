//! A fake of the two providers' OAuth endpoints, shaped like the real ones as
//! they answered on 2026-09-15: Anthropic's JSON token endpoint, and OpenAI's
//! form-encoded token endpoint with its device-code pair beside it.
//!
//! It checks what a real provider checks — the PKCE verifier against the
//! challenge in the authorize URL, the redirect the code was issued for, and a
//! refresh token that has not already been rotated away — so a sign-in that
//! would be refused upstream is refused here too.

use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Form, Json, Router,
};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const GOOD_CODE: &str = "good-code";
pub const DEVICE_CODE: &str = "ABCD-EFGH";
pub const EMAIL: &str = "op@example.com";
pub const ACCOUNT: &str = "acct-1";

#[derive(Default)]
pub struct Inner {
    /// The PKCE challenge and redirect of the sign-in in progress, as the
    /// authorize URL carried them.
    pub challenge: Option<String>,
    pub redirect: Option<String>,
    /// The refresh token that is still good; any other is refused.
    pub live_refresh: Option<String>,
    pub issued: u32,
    pub expires_in: i64,
    pub reject_refresh: bool,
    pub device_approved: bool,
    pub device_denied: bool,
    pub device_polls: u32,
    pub refreshes: u32,
}

#[derive(Clone)]
pub struct OAuthFake {
    pub base: String,
    pub inner: Arc<Mutex<Inner>>,
}

impl OAuthFake {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            expires_in: 2 * 3600,
            ..Default::default()
        }));
        let app = Router::new()
            .route("/anthropic/token", post(anthropic_token))
            .route("/openai/oauth/token", post(openai_token))
            .route("/openai/api/accounts/deviceauth/usercode", post(usercode))
            .route("/openai/api/accounts/deviceauth/token", post(device_token))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { base, inner }
    }

    pub fn endpoints(&self) -> tracon::oauth::Endpoints {
        tracon::oauth::Endpoints {
            anthropic_authorize: "https://claude.example/cai/oauth/authorize".into(),
            anthropic_token: format!("{}/anthropic/token", self.base),
            openai_auth: format!("{}/openai", self.base),
            codex_callback_port: 0,
            device_poll_margin_secs: 0,
        }
    }

    /// Record what the authorize URL asked for, as the provider would when the
    /// operator's browser opened it.
    pub fn authorize(&self, url: &str) -> Authorized {
        let url = reqwest::Url::parse(url).unwrap();
        let pairs: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        let mut inner = self.inner.lock().unwrap();
        inner.challenge = Some(pairs["code_challenge"].clone());
        inner.redirect = Some(pairs["redirect_uri"].clone());
        Authorized {
            state: pairs["state"].clone(),
            redirect: pairs["redirect_uri"].clone(),
            scope: pairs["scope"].clone(),
        }
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.inner.lock().unwrap())
    }
}

pub struct Authorized {
    pub state: String,
    pub redirect: String,
    pub scope: String,
}

fn refused(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

fn verifier_matches(inner: &Inner, verifier: &str) -> bool {
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    inner.challenge.as_deref() == Some(challenge.as_str())
}

fn issue(inner: &mut Inner, prefix: &str) -> (String, String) {
    inner.issued += 1;
    let refresh = format!("{prefix}-refresh-{}", inner.issued);
    inner.live_refresh = Some(refresh.clone());
    (format!("{prefix}-access-{}", inner.issued), refresh)
}

async fn anthropic_token(
    State(inner): State<Arc<Mutex<Inner>>>,
    Json(body): Json<Value>,
) -> Response {
    let mut inner = inner.lock().unwrap();
    match body["grant_type"].as_str() {
        Some("authorization_code") => {
            if body["code"] != GOOD_CODE
                || body["client_id"] != tracon::oauth::anthropic::CLIENT_ID
                || body["redirect_uri"].as_str() != inner.redirect.as_deref()
                || body["state"].as_str().is_none_or(str::is_empty)
                || !verifier_matches(&inner, body["code_verifier"].as_str().unwrap_or(""))
            {
                return refused("invalid_grant");
            }
        }
        Some("refresh_token") => {
            inner.refreshes += 1;
            if inner.reject_refresh
                || body["refresh_token"].as_str() != inner.live_refresh.as_deref()
                || body["scope"] != "user:inference"
            {
                return refused("invalid_grant");
            }
        }
        _ => return refused("unsupported_grant_type"),
    }
    let (access, refresh) = issue(&mut inner, "sk-ant");
    Json(json!({
        "token_type": "Bearer",
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": inner.expires_in,
        "scope": "user:inference",
        "account": { "uuid": "u-1", "email_address": EMAIL },
    }))
    .into_response()
}

pub fn jwt(claims: Value) -> String {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!(
        "{}.{}.sig",
        encode(br#"{"alg":"none"}"#),
        encode(claims.to_string().as_bytes())
    )
}

async fn openai_token(
    State(inner): State<Arc<Mutex<Inner>>>,
    Form(body): Form<std::collections::HashMap<String, String>>,
) -> Response {
    let mut inner = inner.lock().unwrap();
    let field = |name: &str| body.get(name).map(String::as_str).unwrap_or("");
    if field("client_id") != tracon::oauth::codex::CLIENT_ID {
        return refused("invalid_client");
    }
    match field("grant_type") {
        "authorization_code" if field("code") == "device-grant-code" => {
            if field("code_verifier") != "device-verifier"
                || !field("redirect_uri").ends_with("/openai/deviceauth/callback")
            {
                return refused("invalid_grant");
            }
        }
        "authorization_code" => {
            if field("code") != GOOD_CODE
                || Some(field("redirect_uri")) != inner.redirect.as_deref()
                || !verifier_matches(&inner, field("code_verifier"))
            {
                return refused("invalid_grant");
            }
        }
        "refresh_token" => {
            inner.refreshes += 1;
            if inner.reject_refresh || Some(field("refresh_token")) != inner.live_refresh.as_deref()
            {
                return refused("invalid_grant");
            }
        }
        _ => return refused("unsupported_grant_type"),
    }
    let (access, refresh) = issue(&mut inner, "codex");
    let auth = json!({ "chatgpt_account_id": ACCOUNT, "chatgpt_plan_type": "pro" });
    Json(json!({
        "token_type": "Bearer",
        "access_token": jwt(json!({ "https://api.openai.com/auth": auth, "jti": access })),
        "refresh_token": refresh,
        "id_token": jwt(json!({ "email": EMAIL, "https://api.openai.com/auth": auth })),
        "expires_in": inner.expires_in,
        "scope": "openid profile email offline_access",
    }))
    .into_response()
}

async fn usercode(Json(body): Json<Value>) -> Response {
    if body["client_id"] != tracon::oauth::codex::CLIENT_ID {
        return refused("invalid_client");
    }
    Json(json!({
        "device_auth_id": "dev-1",
        "user_code": DEVICE_CODE,
        "interval": "1",
        "expires_at": "2099-01-01T00:00:00Z",
    }))
    .into_response()
}

async fn device_token(State(inner): State<Arc<Mutex<Inner>>>, Json(body): Json<Value>) -> Response {
    let mut inner = inner.lock().unwrap();
    inner.device_polls += 1;
    if body["device_auth_id"] != "dev-1" || body["user_code"] != DEVICE_CODE {
        return StatusCode::NOT_FOUND.into_response();
    }
    if inner.device_denied {
        return refused("access_denied");
    }
    if !inner.device_approved {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "deviceauth_authorization_pending" })),
        )
            .into_response();
    }
    Json(json!({
        "authorization_code": "device-grant-code",
        "code_verifier": "device-verifier",
        "code_challenge": "unused",
        "status": "approved",
        "user_code": DEVICE_CODE,
    }))
    .into_response()
}
