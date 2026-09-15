//! Claude subscription sign-in: Claude Code's OAuth app, asking only for
//! `user:inference`.
//!
//! The token is the short-lived, refreshable kind `claude auth login` mints
//! rather than the year-long one `claude setup-token` asks for with
//! `expires_in`. A refresh rotates both tokens and revokes the access token it
//! replaced at once, so exactly one holder may refresh a given credential.

use super::{expires_at, token_request, Endpoints, OAuthError, Tokens};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// The page that shows `code#state` for the operator to paste back, used when
/// the browser cannot reach this node's loopback.
pub const HOSTED_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
pub const SCOPE: &str = "user:inference";

/// The loopback redirect for a callback listener on `port`. Claude Code's app
/// accepts any port on `localhost`.
pub fn loopback_redirect(port: u16) -> String {
    format!("http://localhost:{port}/callback")
}

pub fn authorize_url(
    endpoints: &Endpoints,
    redirect: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut url = reqwest::Url::parse(&endpoints.anthropic_authorize).expect("authorize URL");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

pub async fn exchange(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    code: &str,
    state: &str,
    redirect: &str,
    verifier: &str,
) -> Result<Tokens, OAuthError> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect,
        "client_id": CLIENT_ID,
        "code_verifier": verifier,
        "state": state,
    });
    let response = token_request(
        client.post(&endpoints.anthropic_token).json(&body),
        "the Anthropic sign-in",
    )
    .await?;
    tokens(&response)
}

pub async fn refresh(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    refresh_token: &str,
) -> Result<Tokens, OAuthError> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
        "scope": SCOPE,
    });
    let response = token_request(
        client.post(&endpoints.anthropic_token).json(&body),
        "the Anthropic token refresh",
    )
    .await?;
    tokens(&response)
}

fn tokens(response: &serde_json::Value) -> Result<Tokens, OAuthError> {
    let access = response["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            OAuthError::Unavailable("the Anthropic token response had no access token".into())
        })?;
    Ok(Tokens {
        access: access.to_string(),
        refresh: response["refresh_token"].as_str().map(str::to_string),
        expires_ms: expires_at(response["expires_in"].as_i64()),
        identity: response["account"]["email_address"]
            .as_str()
            .map(str::to_string),
        account_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_asks_for_inference_only_with_pkce() {
        let url = authorize_url(
            &Endpoints::default(),
            &loopback_redirect(54545),
            "challenge",
            "state-1",
        );
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("claude.com"));
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], CLIENT_ID);
        assert_eq!(pairs["redirect_uri"], "http://localhost:54545/callback");
        assert_eq!(pairs["scope"], "user:inference");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["state"], "state-1");
        assert!(!pairs.contains_key("expires_in"));
    }

    #[test]
    fn a_token_response_is_read_without_an_account_id() {
        let tokens = tokens(&serde_json::json!({
            "access_token": "sk-ant-oat01-x",
            "refresh_token": "sk-ant-ort01-y",
            "expires_in": 28800,
            "scope": "user:inference",
            "account": {"uuid": "u", "email_address": "op@example.com"},
        }))
        .unwrap();
        assert_eq!(tokens.access, "sk-ant-oat01-x");
        assert_eq!(tokens.refresh.as_deref(), Some("sk-ant-ort01-y"));
        assert_eq!(tokens.identity.as_deref(), Some("op@example.com"));
        assert!(tokens.expires_ms.unwrap() > crate::store::now_ms() + 28_000_000);
        assert!(tokens.account_id.is_none());
        assert!(super::tokens(&serde_json::json!({"refresh_token": "y"})).is_err());
    }
}
