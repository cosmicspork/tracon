//! ChatGPT subscription sign-in for Codex: the Codex CLI's OAuth app, by
//! browser callback on a local node and by device code anywhere else.
//!
//! The access token lasts ten days; a refresh rotates the refresh token. The
//! account id every Codex request carries is read from the tokens' claims.

use super::{expires_at, jwt_claims, token_request, Endpoints, OAuthError, Tokens};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_BASE: &str = "https://auth.openai.com";
/// The one loopback port OpenAI's app registers for its browser callback.
pub const CALLBACK_PORT: u16 = 1455;
pub const SCOPE: &str = "openid profile email offline_access";
/// The `originator` OpenAI's app was verified to accept with this client id.
const ORIGINATOR: &str = "opencode";

pub fn loopback_redirect(port: u16) -> String {
    format!("http://localhost:{port}/auth/callback")
}

pub fn device_page(endpoints: &Endpoints) -> String {
    format!("{}/codex/device", endpoints.openai_auth)
}

pub fn authorize_url(
    endpoints: &Endpoints,
    redirect: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", endpoints.openai_auth))
        .expect("authorize URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", ORIGINATOR);
    url.to_string()
}

pub async fn exchange(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> Result<Tokens, OAuthError> {
    let response = token_request(
        form(
            client,
            endpoints,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier),
            ],
        ),
        "the ChatGPT sign-in",
    )
    .await?;
    tokens(&response)
}

pub async fn refresh(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    refresh_token: &str,
) -> Result<Tokens, OAuthError> {
    let response = token_request(
        form(
            client,
            endpoints,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ],
        ),
        "the ChatGPT token refresh",
    )
    .await?;
    tokens(&response)
}

/// A device sign-in in progress: the code the operator enters, and what the
/// node polls with.
#[derive(Clone)]
pub struct DeviceStart {
    pub user_code: String,
    pub device_auth_id: String,
    pub interval: std::time::Duration,
}

pub async fn start_device(
    client: &reqwest::Client,
    endpoints: &Endpoints,
) -> Result<DeviceStart, OAuthError> {
    let response = token_request(
        client
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                endpoints.openai_auth
            ))
            .json(&serde_json::json!({ "client_id": CLIENT_ID })),
        "the ChatGPT device sign-in",
    )
    .await?;
    let field = |name: &str| {
        response[name]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                OAuthError::Unavailable(format!("the device sign-in response had no {name}"))
            })
    };
    // OpenAI sends the interval as a string; a number is accepted too.
    let interval = response["interval"]
        .as_u64()
        .or_else(|| {
            response["interval"]
                .as_str()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(5)
        .max(1);
    Ok(DeviceStart {
        user_code: field("user_code")?,
        device_auth_id: field("device_auth_id")?,
        interval: std::time::Duration::from_secs(interval),
    })
}

/// One poll. `None` while the operator has not approved yet, which OpenAI
/// answers with 403 or 404; the grant once they have.
pub async fn poll_device(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    device: &DeviceStart,
) -> Result<Option<Tokens>, OAuthError> {
    let response = client
        .post(format!(
            "{}/api/accounts/deviceauth/token",
            endpoints.openai_auth
        ))
        .json(&serde_json::json!({
            "device_auth_id": device.device_auth_id,
            "user_code": device.user_code,
        }))
        .send()
        .await
        .map_err(|error| {
            OAuthError::Unavailable(format!(
                "the device sign-in poll: {}",
                super::without_url(error)
            ))
        })?;
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(OAuthError::Rejected(format!(
            "the device sign-in was refused ({status})"
        )));
    }
    let grant: serde_json::Value = response
        .json()
        .await
        .map_err(|_| OAuthError::Unavailable("the device sign-in grant was not JSON".into()))?;
    let (Some(code), Some(verifier)) = (
        grant["authorization_code"].as_str(),
        grant["code_verifier"].as_str(),
    ) else {
        return Err(OAuthError::Unavailable(
            "the device sign-in grant had no authorization code".into(),
        ));
    };
    let redirect = format!("{}/deviceauth/callback", endpoints.openai_auth);
    exchange(client, endpoints, code, &redirect, verifier)
        .await
        .map(Some)
}

fn form(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    pairs: &[(&str, &str)],
) -> reqwest::RequestBuilder {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    client
        .post(format!("{}/oauth/token", endpoints.openai_auth))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
}

fn tokens(response: &serde_json::Value) -> Result<Tokens, OAuthError> {
    let access = response["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            OAuthError::Unavailable("the ChatGPT token response had no access token".into())
        })?;
    let id_claims = response["id_token"].as_str().and_then(jwt_claims);
    let access_claims = jwt_claims(access);
    let account_id = id_claims
        .as_ref()
        .and_then(account_id)
        .or_else(|| access_claims.as_ref().and_then(account_id));
    let identity = id_claims
        .as_ref()
        .and_then(|claims| claims["email"].as_str())
        .map(str::to_string);
    Ok(Tokens {
        access: access.to_string(),
        refresh: response["refresh_token"].as_str().map(str::to_string),
        expires_ms: expires_at(response["expires_in"].as_i64()),
        identity,
        account_id,
    })
}

fn account_id(claims: &serde_json::Value) -> Option<String> {
    claims["chatgpt_account_id"]
        .as_str()
        .or_else(|| claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str())
        .or_else(|| claims["organizations"][0]["id"].as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn jwt(claims: serde_json::Value) -> String {
        let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
        format!(
            "{}.{}.sig",
            encode(br#"{"alg":"none"}"#),
            encode(claims.to_string().as_bytes())
        )
    }

    #[test]
    fn the_account_id_and_address_come_from_the_id_token_then_the_access_token() {
        let tokens = tokens(&serde_json::json!({
            "access_token": jwt(serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": "from-access"}})),
            "id_token": jwt(serde_json::json!({
                "email": "op@example.com",
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"},
            })),
            "refresh_token": "rt",
            "expires_in": 864000,
        }))
        .unwrap();
        assert_eq!(tokens.account_id.as_deref(), Some("acct-1"));
        assert_eq!(tokens.identity.as_deref(), Some("op@example.com"));

        let refreshed = super::tokens(&serde_json::json!({
            "access_token": jwt(serde_json::json!({"https://api.openai.com/auth": {"chatgpt_account_id": "from-access"}})),
            "expires_in": 864000,
        }))
        .unwrap();
        assert_eq!(refreshed.account_id.as_deref(), Some("from-access"));
        assert!(refreshed.identity.is_none());
        assert!(refreshed.refresh.is_none());
    }

    #[test]
    fn the_authorize_url_uses_the_one_registered_loopback() {
        let url = authorize_url(
            &Endpoints::default(),
            &loopback_redirect(CALLBACK_PORT),
            "challenge",
            "state-1",
        );
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert_eq!(parsed.path(), "/oauth/authorize");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], CLIENT_ID);
        assert_eq!(pairs["redirect_uri"], "http://localhost:1455/auth/callback");
        assert_eq!(pairs["scope"], SCOPE);
        assert_eq!(pairs["code_challenge_method"], "S256");
    }
}
