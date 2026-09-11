//! Candidate-bound QA deployment and browser verification primitives.
//!
//! The HTTP and MCP entrypoints both use this module. They never turn an agent
//! request into a command line: target configuration supplies the deployment
//! transport and browser image, while inputs are small structured assertions.

use std::collections::BTreeSet;

use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::{qa_origin, QaTarget},
    store::{BrowserRunRow, QaDeploymentRow},
};
pub mod service;

pub const DEPLOY_ACTION: &str = "deploy";
pub const BROWSER_VERIFY_ACTION: &str = "browser_verify";
pub const BROWSER_TEST_ACCOUNT_ACTION: &str = "browser_test_account";
pub const MAX_STEPS: usize = 32;
pub const MAX_ASSERTIONS: usize = 32;
pub const MAX_SELECTOR: usize = 512;
pub const MAX_TEXT: usize = 4 * 1024;
pub const MAX_LOG_BYTES: usize = 64 * 1024;
pub const MAX_SCREENSHOTS: usize = 8;

/// A request names a stored candidate and an operator-configured target, never
/// an image, command, arbitrary deployment URL, or browser executable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployRequest {
    pub candidate_id: String,
    pub target: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserRequest {
    pub deployment_id: String,
    pub scenario: BrowserScenario,
}

/// The browser's declarative surface. No JavaScript is accepted: allowing it
/// would make the browser image a credential-bearing arbitrary code runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserScenario {
    pub start_path: String,
    #[serde(default)]
    pub steps: Vec<BrowserStep>,
    #[serde(default)]
    pub assertions: Vec<BrowserAssertion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserStep {
    Navigate { path: String },
    Click { selector: String },
    /// `credential_env` is the *name* of a key inside the configured dedicated
    /// test-account broker entry. The value is never accepted from, returned to,
    /// or persisted by this API.
    Fill {
        selector: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        credential_env: Option<String>,
    },
    WaitFor { selector: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BrowserAssertion {
    UrlPathIs { path: String },
    TitleContains { text: String },
    TextVisible { selector: String, text: String },
    ElementCount { selector: String, count: u16 },
    Screenshot { label: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowserPlan {
    pub start_url: String,
    pub allowed_origins: Vec<String>,
    pub steps: Vec<BrowserStep>,
    pub assertions: Vec<BrowserAssertion>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentObservation {
    pub identity: Option<String>,
    pub state: &'static str,
    pub detail: String,
    pub observed_ms: i64,
}

pub fn deploy_authority_target(target_id: &str, target: &QaTarget) -> String {
    format!(
        "gitlab:{}:environment:{}",
        target.deployment.project.trim(),
        target_id
    )
}

pub fn browser_authority_target(target_id: &str, target: &QaTarget) -> Result<String, String> {
    Ok(format!("qa:{target_id}:origin:{}", qa_origin(&target.origin)?))
}

pub fn test_account_authority_target(target_id: &str, credential: &str) -> String {
    format!("qa:{target_id}:credential:{credential}")
}

pub fn browser_plan(target: &QaTarget, scenario: &BrowserScenario) -> Result<BrowserPlan, String> {
    let origin = qa_origin(&target.origin)?;
    let start_url = browser_url(&origin, &scenario.start_path)?;
    if scenario.steps.len() > MAX_STEPS {
        return Err(format!("browser scenario has more than {MAX_STEPS} steps"));
    }
    if scenario.assertions.len() > MAX_ASSERTIONS {
        return Err(format!("browser scenario has more than {MAX_ASSERTIONS} assertions"));
    }
    let mut screenshots = 0usize;
    for step in &scenario.steps {
        match step {
            BrowserStep::Navigate { path } => {
                browser_url(&origin, path)?;
            }
            BrowserStep::Click { selector } | BrowserStep::WaitFor { selector } => selector_ok(selector)?,
            BrowserStep::Fill {
                selector,
                value,
                credential_env,
            } => {
                selector_ok(selector)?;
                if value.is_some() == credential_env.is_some() {
                    return Err("browser fill needs exactly one of value or credential_env".into());
                }
                if let Some(value) = value {
                    text_ok(value, "browser fill value")?;
                }
                if let Some(key) = credential_env {
                    env_key_ok(key)?;
                }
            }
        }
    }
    for assertion in &scenario.assertions {
        match assertion {
            BrowserAssertion::UrlPathIs { path } => {
                browser_url(&origin, path)?;
            }
            BrowserAssertion::TitleContains { text } => text_ok(text, "browser title assertion")?,
            BrowserAssertion::TextVisible { selector, text } => {
                selector_ok(selector)?;
                text_ok(text, "browser text assertion")?;
            }
            BrowserAssertion::ElementCount { selector, .. } => selector_ok(selector)?,
            BrowserAssertion::Screenshot { label } => {
                screenshots += 1;
                if screenshots > MAX_SCREENSHOTS {
                    return Err(format!("browser scenario has more than {MAX_SCREENSHOTS} screenshots"));
                }
                label_ok(label)?;
            }
        }
    }
    Ok(BrowserPlan {
        start_url,
        allowed_origins: target.canonical_origins()?,
        steps: scenario.steps.clone(),
        assertions: scenario.assertions.clone(),
    })
}

pub fn requested_credential_keys(scenario: &BrowserScenario) -> BTreeSet<String> {
    scenario
        .steps
        .iter()
        .filter_map(|step| match step {
            BrowserStep::Fill { credential_env, .. } => credential_env.clone(),
            _ => None,
        })
        .collect()
}

/// Revalidate a target without following redirects. A redirect could be a
/// deployment change or an unauthorised origin, neither of which may quietly
/// borrow the old identity.
pub async fn observe_environment(target: &QaTarget) -> EnvironmentObservation {
    let observed_ms = crate::store::now_ms();
    let endpoint = match target.identity_endpoint() {
        Ok(endpoint) => endpoint,
        Err(detail) => {
            return EnvironmentObservation {
                identity: None,
                state: "unknown",
                detail,
                observed_ms,
            }
        }
    };
    let header = match reqwest::header::HeaderName::from_bytes(target.identity_header.as_bytes()) {
        Ok(header) => header,
        Err(_) => {
            return EnvironmentObservation {
                identity: None,
                state: "unknown",
                detail: "configured identity header is malformed".into(),
                observed_ms,
            }
        }
    };
    let client = match reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return EnvironmentObservation {
                identity: None,
                state: "unknown",
                detail: format!("cannot build identity client: {error}"),
                observed_ms,
            }
        }
    };
    match client.head(endpoint).send().await {
        Ok(response) if response.status().is_success() => match response.headers().get(header) {
            Some(value) => match value.to_str().ok().map(str::trim).filter(|v| identity_ok(v)) {
                Some(identity) => EnvironmentObservation {
                    identity: Some(identity.to_string()),
                    state: "fresh",
                    detail: "identity endpoint observed".into(),
                    observed_ms,
                },
                None => EnvironmentObservation {
                    identity: None,
                    state: "unknown",
                    detail: "identity endpoint returned an invalid identity".into(),
                    observed_ms,
                },
            },
            None => EnvironmentObservation {
                identity: None,
                state: "unknown",
                detail: "identity endpoint omitted the configured identity header".into(),
                observed_ms,
            },
        },
        Ok(response) => EnvironmentObservation {
            identity: None,
            state: "unknown",
            detail: format!("identity endpoint returned {}", response.status()),
            observed_ms,
        },
        Err(error) => EnvironmentObservation {
            identity: None,
            state: "unknown",
            detail: format!("identity endpoint was not observable: {error}"),
            observed_ms,
        },
    }
}

/// A browser run is fresh only when both revalidations saw exactly the
/// deployment's identity and the newest later target observation agrees. A
/// missing identity is not treated as equality with another missing identity.
pub fn evidence_state(
    deployment: &QaDeploymentRow,
    run: &BrowserRunRow,
    newest_target_observation: Option<&QaDeploymentRow>,
) -> &'static str {
    if deployment.outcome != "succeeded" {
        return "unknown";
    }
    if deployment.identity_state != "fresh"
        || run.environment_before.is_none()
        || run.environment_after.is_none()
        || run.environment_before != deployment.environment_identity
        || run.environment_after != deployment.environment_identity
    {
        return "unknown";
    }
    match newest_target_observation {
        Some(newest)
            if newest.identity_state == "fresh"
                && newest.environment_identity == deployment.environment_identity => "fresh",
        Some(newest) if newest.identity_state == "fresh" => "stale",
        Some(_) => "unknown",
        None => "unknown",
    }
}

/// Bound and redact runner output before it becomes durable evidence. Values
/// are supplied solely from the dedicated browser credential and are never
/// returned to callers, even if a page or browser wrote one into its console.
pub fn bounded_redacted_log(raw: &[u8], secret_values: impl IntoIterator<Item = String>) -> String {
    let start = raw.len().saturating_sub(MAX_LOG_BYTES);
    let mut text = String::from_utf8_lossy(&raw[start..]).into_owned();
    for value in secret_values {
        if !value.is_empty() {
            text = text.replace(&value, "[redacted]");
        }
    }
    text
}

pub fn browser_url(origin: &str, path: &str) -> Result<String, String> {
    if path.len() > 2048
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['\0', '\r', '\n'])
    {
        return Err("browser paths must be bounded absolute paths on the configured origin".into());
    }
    let mut url = url::Url::parse(origin).map_err(|e| format!("configured origin is invalid: {e}"))?;
    url.set_path(path);
    url.set_query(None);
    url.set_fragment(None);
    if url.origin().ascii_serialization() != origin {
        return Err("browser path escaped the configured origin".into());
    }
    Ok(url.to_string())
}

fn selector_ok(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_SELECTOR || value.contains(['\0', '\r', '\n']) {
        Err("browser selector is empty, oversized, or contains a control character".into())
    } else {
        Ok(())
    }
}

fn text_ok(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_TEXT || value.contains('\0') {
        Err(format!("{label} is empty, oversized, or contains a NUL"))
    } else {
        Ok(())
    }
}

fn env_key_ok(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_uppercase() || (index > 0 && byte.is_ascii_digit())
        })
    {
        Err("browser credential_env is not an environment key".into())
    } else {
        Ok(())
    }
}

fn label_ok(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Err("screenshot labels must be lowercase letters, digits, or dashes".into())
    } else {
        Ok(())
    }
}

fn identity_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
}

/// The program placed into a trusted runtime volume before a browser run. It
/// uses Playwright's network interception for every request and WebSocket;
/// callers also set the container's proxy/boundary, giving defence in depth.
pub const BROWSER_RUNNER: &str = include_str!("qa/browser-runner.cjs");

/// Parse the browser's bounded JSON report. The report is only accepted after
/// shape checks, so a compromised page cannot claim arbitrary persisted state.
pub fn browser_report(value: Value) -> Result<BrowserReport, String> {
    let report: BrowserReport = serde_json::from_value(value)
        .map_err(|error| format!("browser runtime returned malformed report: {error}"))?;
    if report.assertions.len() > MAX_ASSERTIONS || report.screenshots.len() > MAX_SCREENSHOTS {
        return Err("browser runtime report exceeds evidence limits".into());
    }
    if report.log.len() > MAX_LOG_BYTES {
        return Err("browser runtime report log exceeds evidence limit".into());
    }
    for shot in &report.screenshots {
        label_ok(&shot.label)?;
        if !crate::config::safe_relative_path(&shot.path)
            || !shot.path.starts_with("output/screenshots/")
            || !shot.path.ends_with(".png")
        {
            return Err("browser screenshot path is invalid".into());
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserReport {
    pub ok: bool,
    pub final_url: String,
    pub assertions: Vec<BrowserAssertionResult>,
    pub screenshots: Vec<BrowserScreenshot>,
    pub log: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserAssertionResult {
    pub kind: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserScreenshot {
    pub label: String,
    /// Relative to the browser workspace's exported immutable snapshot.
    pub path: String,
}
