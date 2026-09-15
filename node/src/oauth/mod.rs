//! Subscription sign-in, run by the node itself.
//!
//! Each provider's subscription is minted by its own first-party client's
//! OAuth app: Claude Code's for Anthropic, the Codex CLI's (as OpenCode uses
//! it) for ChatGPT. The node speaks those flows directly rather than driving
//! the CLIs, so the only thing that crosses a process boundary is HTTPS to the
//! provider. The client ids and endpoints are the providers' own and change
//! only when they do; `docs/ARCHITECTURE.md` records where each was read from.

pub mod anthropic;
pub mod codex;

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::store::now_ms;

const USER_AGENT: &str = concat!("tracon/", env!("CARGO_PKG_VERSION"));
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Where each flow's requests go. Production uses [`Endpoints::default`];
/// tests point every field at a fake provider.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub anthropic_authorize: String,
    pub anthropic_token: String,
    /// `https://auth.openai.com`: authorize, token, and the device endpoints
    /// all hang off it.
    pub openai_auth: String,
    /// The loopback port OpenAI's app accepts for a browser callback. The app
    /// registers exactly one, so a node cannot choose; zero asks the kernel,
    /// which only a fake provider accepts.
    pub codex_callback_port: u16,
    /// Seconds added to the device flow's own poll interval, as OpenAI's own
    /// clients do. Zero in tests.
    pub device_poll_margin_secs: u64,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            anthropic_authorize: anthropic::AUTHORIZE_URL.into(),
            anthropic_token: anthropic::TOKEN_URL.into(),
            openai_auth: codex::AUTH_BASE.into(),
            codex_callback_port: codex::CALLBACK_PORT,
            device_poll_margin_secs: 3,
        }
    }
}

/// Which sign-in a provider's `login` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Anthropic,
    Codex,
}

impl Flow {
    /// `openai-codex` is the id the retired harness wrote into `node.toml`;
    /// OpenCode's `openai` replaced it, and both still mean this flow.
    pub fn for_login(id: &str) -> Option<Self> {
        match id {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "openai-codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// What a sign-in or refresh returns, as the broker keeps it.
#[derive(Clone, Default, PartialEq)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_ms: Option<i64>,
    pub identity: Option<String>,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tokens")
            .field("access", &"<redacted>")
            .field("refresh", &self.refresh.as_ref().map(|_| "<redacted>"))
            .field("expires_ms", &self.expires_ms)
            .field("identity", &self.identity)
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The provider refused the grant itself: the code was used or expired,
    /// or the refresh token was revoked or already rotated away. Nothing
    /// retried will change that.
    #[error("{0}")]
    Rejected(String),
    /// Anything that might go differently next time.
    #[error("{0}")]
    Unavailable(String),
}

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = random_token(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

pub fn random_token(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut raw);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("OAuth HTTP client")
}

/// What the operator pasted back: a redirect URL carrying `code` and `state`,
/// the `code#state` Anthropic's hosted page shows, or a bare code.
#[derive(Debug, PartialEq, Eq)]
pub struct Pasted {
    pub code: String,
    pub state: Option<String>,
}

pub fn parse_pasted(text: &str) -> Option<Pasted> {
    let text = text.trim();
    if let Ok(url) = reqwest::Url::parse(text) {
        if url.scheme() == "http" || url.scheme() == "https" {
            let code = url
                .query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, value)| value.into_owned())?;
            let state = url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned());
            return (!code.is_empty()).then_some(Pasted { code, state });
        }
    }
    let (code, state) = match text.split_once('#') {
        Some((code, state)) => (code, Some(state.to_string())),
        None => (text, None),
    };
    (!code.is_empty() && !code.contains(char::is_whitespace)).then(|| Pasted {
        code: code.to_string(),
        state,
    })
}

pub(crate) fn expires_at(expires_in: Option<i64>) -> Option<i64> {
    expires_in.map(|seconds| now_ms() + seconds * 1000)
}

/// The claims of a JWT, unverified: read for the account id and address the
/// provider put there, never trusted for anything the provider does not check
/// again on every request.
pub(crate) fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Send a token request and read its JSON body, sorting a refused grant from a
/// failure that could go differently next time. The provider's error body is
/// reduced to its `error` code: a description can echo what was sent.
pub(crate) async fn token_request(
    request: reqwest::RequestBuilder,
    what: &str,
) -> Result<serde_json::Value, OAuthError> {
    let response = request
        .send()
        .await
        .map_err(|error| OAuthError::Unavailable(format!("{what}: {}", without_url(error))))?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    let code = body["error"]
        .as_str()
        .or_else(|| body["error"]["type"].as_str())
        .unwrap_or("no error code");
    let message = format!("{what} was refused ({status}, {code})");
    if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED {
        Err(OAuthError::Rejected(message))
    } else {
        Err(OAuthError::Unavailable(message))
    }
}

pub(crate) fn without_url(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_s256_of_the_verifier() {
        let pkce = Pkce::new();
        assert!(pkce.verifier.len() >= 43);
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn a_paste_is_read_as_a_redirect_a_hosted_code_or_a_bare_code() {
        assert_eq!(
            parse_pasted("http://localhost:1455/auth/callback?code=a%2Fb&state=s1&scope=x"),
            Some(Pasted {
                code: "a/b".into(),
                state: Some("s1".into())
            })
        );
        assert_eq!(
            parse_pasted("  abc123#st4te \n"),
            Some(Pasted {
                code: "abc123".into(),
                state: Some("st4te".into())
            })
        );
        assert_eq!(
            parse_pasted("abc123"),
            Some(Pasted {
                code: "abc123".into(),
                state: None
            })
        );
        for refused in ["", "#state", "https://example.com/?state=s", "two words"] {
            assert_eq!(parse_pasted(refused), None, "{refused}");
        }
    }

    #[test]
    fn token_debug_output_redacts_secrets() {
        let tokens = Tokens {
            access: "secret-access".into(),
            refresh: Some("secret-refresh".into()),
            expires_ms: Some(1),
            identity: Some("op@example".into()),
            account_id: Some("secret-account".into()),
        };
        let shown = format!("{tokens:?}");
        assert!(!shown.contains("secret"));
        assert!(shown.contains("op@example"));
    }

    #[test]
    fn a_login_id_names_its_flow() {
        assert_eq!(Flow::for_login("anthropic"), Some(Flow::Anthropic));
        assert_eq!(Flow::for_login("openai"), Some(Flow::Codex));
        assert_eq!(Flow::for_login("openai-codex"), Some(Flow::Codex));
        assert_eq!(Flow::for_login("copilot"), None);
    }
}
