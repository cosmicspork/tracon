//! A client for DigitalOcean's Harness Runtime API (`/v2/harness-runtime`),
//! the elastic sandbox provider in the sandbox protocol plan: sessions and
//! their agent configs, pause and resume, fork, one-shot exec, checkpoints,
//! custom templates, and the cleanup the API leaves to its caller.
//!
//! Only the bare-sandbox path is covered (`harness: none`): the node runs its
//! own harness inside the sandbox, so DigitalOcean's managed agents, prompts,
//! events and approval requests are out of scope.
//!
//! The token is the caller's, read from the broker on the node's side of the
//! privilege boundary and handed in; this module never looks it up. Requests
//! carrying it never follow redirects.
//!
//! Things learned against the live API (2026-11-02) that the shapes alone do
//! not say:
//! - A session created from a manifest also stores an agent config named
//!   after it, and removing the session leaves that config behind
//!   (`cleanup::leftover_configs`).
//! - Placement follows the API host (`api-<region>.digitalocean.com`); a
//!   manifest with a `region` field is refused 400 as an unknown field.
//! - The manifest endpoint takes `application/x-yaml`, and JSON is YAML: a
//!   JSON body is parsed the same (it drew the same unknown-field refusals),
//!   so no YAML encoder is needed.
//! - Implicit checkpoints are recorded at 51 GB each and are kept until
//!   deleted (`cleanup::plan_checkpoint_retention`).

pub mod cleanup;
pub mod types;

use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

pub use types::*;

pub const DEFAULT_BASE_URL: &str = "https://api.digitalocean.com";
/// The env name doctl reads its token from: what a broker credential for
/// this API is expected to carry.
pub const TOKEN_ENV: &str = "DIGITALOCEAN_ACCESS_TOKEN";
/// The most children one fork may create.
pub const FORK_MAX_COUNT: u8 = 4;

const PREFIX: &str = "/v2/harness-runtime";
const MANIFEST_MEDIA_TYPE: &str = "application/x-yaml";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Creating a checkpoint blocks until it is READY, and a fork may take one
/// first; a sandbox's memory and disk can be tens of gigabytes.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Added to an exec's own budget so the server's 504 arrives before ours.
const EXEC_MARGIN: Duration = Duration::from_secs(30);
/// The server's buffered-exec default when none is asked for.
const EXEC_DEFAULT: Duration = Duration::from_secs(4 * 60);
/// Guards against a server that hands back the same page token forever.
const MAX_PAGES: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum DoError {
    #[error("DigitalOcean {what}: {source}")]
    Transport {
        what: &'static str,
        #[source]
        source: reqwest::Error,
    },
    /// A non-2xx answer, with the API's own reason when it gave one.
    #[error("DigitalOcean {what} answered {status}: {message}")]
    Api {
        what: &'static str,
        status: u16,
        /// The API's error code, e.g. `bad_request` or `not_found`.
        id: Option<String>,
        message: String,
        request_id: Option<String>,
    },
    #[error("DigitalOcean {what}: unreadable response: {detail}")]
    Decode { what: &'static str, detail: String },
    /// Refused before anything was sent.
    #[error("{0}")]
    Invalid(String),
}

impl DoError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    /// Worth trying again later: rate limiting, a server-side failure, or no
    /// answer at all.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    /// The field a manifest was refused for, from the API's
    /// `unknown field "region"` refusal.
    pub fn unknown_field(&self) -> Option<&str> {
        let Self::Api {
            status: 400,
            message,
            ..
        } = self
        else {
            return None;
        };
        let rest = &message[message.find("unknown field \"")? + "unknown field \"".len()..];
        Some(&rest[..rest.find('"')?])
    }
}

/// Filters for listing sessions.
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub name: Option<String>,
    pub status: Option<SessionStatus>,
    /// Lists the children forked from this session.
    pub parent_session_id: Option<String>,
}

/// The ways the API spells a refusal: `{id, message, request_id}` from the
/// front door, `{error: {code, message}}` from the harness service behind
/// it, and the `{errors: [...]}` list doctl prints.
#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    id: Option<String>,
    message: Option<String>,
    request_id: Option<String>,
    error: Option<NestedError>,
    #[serde(default)]
    errors: Vec<ListedError>,
}

#[derive(Debug, Deserialize)]
struct NestedError {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListedError {
    detail: Option<String>,
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct SessionRoot {
    session: Option<Session>,
}

#[derive(Deserialize)]
struct SessionsPage {
    #[serde(default)]
    sessions: Vec<Session>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct ConfigRoot {
    config: Option<Config>,
}

#[derive(Deserialize)]
struct ConfigsPage {
    #[serde(default)]
    configs: Vec<ConfigSummary>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct CheckpointRoot {
    checkpoint: Option<Checkpoint>,
}

#[derive(Deserialize)]
struct CheckpointsPage {
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct TemplateRoot {
    template: Option<Template>,
}

#[derive(Deserialize)]
struct TemplatesPage {
    #[serde(default)]
    templates: Vec<Template>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct BuildRoot {
    build: Option<TemplateBuild>,
}

#[derive(Deserialize)]
struct BuildsPage {
    #[serde(default)]
    builds: Vec<TemplateBuild>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct SizesRoot {
    #[serde(default)]
    sizes: Vec<Size>,
}

#[derive(Deserialize)]
struct BalanceRoot {
    status: Option<Balance>,
}

/// One page of a listing: its items and the token for the next page, empty
/// on the last.
trait Page {
    type Item;
    fn split(self) -> (Vec<Self::Item>, Option<String>);
}

macro_rules! page {
    ($page:ty, $field:ident, $item:ty) => {
        impl Page for $page {
            type Item = $item;
            fn split(self) -> (Vec<$item>, Option<String>) {
                (self.$field, self.next_page_token.filter(|t| !t.is_empty()))
            }
        }
    };
}

page!(SessionsPage, sessions, Session);
page!(ConfigsPage, configs, ConfigSummary);
page!(CheckpointsPage, checkpoints, Checkpoint);
page!(TemplatesPage, templates, Template);
page!(BuildsPage, builds, TemplateBuild);

/// A Harness Runtime API client bound to one API host and one token.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for the default region's host.
    pub fn new(token: impl Into<String>) -> Result<Self, DoError> {
        Self::with_base_url(DEFAULT_BASE_URL, token)
    }

    /// A client for one region's host: what places a session there, and the
    /// only host that will find it afterwards.
    pub fn regional(region: &str, token: impl Into<String>) -> Result<Self, DoError> {
        let region = region.trim().to_ascii_lowercase();
        if region.is_empty() {
            return Self::new(token);
        }
        if !region.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(DoError::Invalid(format!("not a region slug: {region:?}")));
        }
        Self::with_base_url(&format!("https://api-{region}.digitalocean.com"), token)
    }

    /// A client for any host, such as a test's loopback server.
    pub fn with_base_url(base: &str, token: impl Into<String>) -> Result<Self, DoError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(DoError::Invalid("the DigitalOcean token is empty".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|source| DoError::Transport {
                what: "client setup",
                source,
            })?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// The token it was made with: DigitalOcean's container registry takes
    /// the same one, for a push to it.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    // ---- sessions ----

    /// Creates a session from a manifest. The API also stores an agent
    /// config named after the session, which outlives it.
    pub async fn create_session(&self, manifest: &Manifest) -> Result<Session, DoError> {
        if manifest.name.trim().is_empty() || manifest.harness.trim().is_empty() {
            return Err(DoError::Invalid(
                "a session manifest needs a name and a harness".into(),
            ));
        }
        let body = serde_json::to_vec(manifest).map_err(|e| DoError::Invalid(e.to_string()))?;
        let req = self
            .request(Method::POST, &format!("{PREFIX}/sessions"))
            .header(CONTENT_TYPE, MANIFEST_MEDIA_TYPE)
            .body(body);
        self.create_session_with(req).await
    }

    /// Creates a session from an existing agent config, which stores nothing
    /// new.
    pub async fn create_session_from_config(
        &self,
        name: &str,
        config_id: &str,
    ) -> Result<Session, DoError> {
        if name.trim().is_empty() {
            return Err(DoError::Invalid("a session needs a name".into()));
        }
        segment(config_id)?;
        let req = self
            .request(Method::POST, &format!("{PREFIX}/sessions"))
            .json(&json!({ "name": name, "config_id": config_id }));
        self.create_session_with(req).await
    }

    async fn create_session_with(&self, req: RequestBuilder) -> Result<Session, DoError> {
        const WHAT: &str = "session create";
        let response = req
            .send()
            .await
            .map_err(|source| DoError::Transport { what: WHAT, source })?;
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let root: SessionRoot = read(response, WHAT).await?;
        let mut session = root.session.ok_or_else(|| missing(WHAT, "session"))?;
        session.location = location;
        Ok(session)
    }

    pub async fn get_session(&self, session_id: &str) -> Result<Session, DoError> {
        const WHAT: &str = "session get";
        let path = format!("{PREFIX}/sessions/{}", segment(session_id)?);
        let root: SessionRoot = self.send(self.request(Method::GET, &path), WHAT).await?;
        root.session.ok_or_else(|| missing(WHAT, "session"))
    }

    /// Every session the filter matches, across pages.
    pub async fn list_sessions(&self, filter: &SessionFilter) -> Result<Vec<Session>, DoError> {
        let mut query = Vec::new();
        if let Some(name) = &filter.name {
            query.push(("name", name.clone()));
        }
        if let Some(status) = &filter.status {
            query.push(("status", status.as_str().to_string()));
        }
        if let Some(parent) = &filter.parent_session_id {
            query.push(("parent_session_id", parent.clone()));
        }
        self.list_all::<SessionsPage>(&format!("{PREFIX}/sessions"), query, "session list")
            .await
    }

    /// The session with exactly this name, if there is one.
    pub async fn find_session(&self, name: &str) -> Result<Option<Session>, DoError> {
        let filter = SessionFilter {
            name: Some(name.to_string()),
            ..SessionFilter::default()
        };
        Ok(self
            .list_sessions(&filter)
            .await?
            .into_iter()
            .find(|s| s.name == name))
    }

    /// Removes a session and its sandbox. Its agent config stays; see
    /// `cleanup`. A session already gone is not an error.
    pub async fn delete_session(&self, session_id: &str) -> Result<(), DoError> {
        let path = format!("{PREFIX}/sessions/{}", segment(session_id)?);
        self.send_idempotent_delete(self.request(Method::DELETE, &path), "session delete")
            .await
    }

    pub async fn pause_session(&self, session_id: &str) -> Result<(), DoError> {
        let path = format!("{PREFIX}/sessions/{}/pause", segment(session_id)?);
        self.send_empty(
            self.request(Method::POST, &path).json(&json!({})),
            "session pause",
        )
        .await
    }

    pub async fn resume_session(&self, session_id: &str) -> Result<(), DoError> {
        let path = format!("{PREFIX}/sessions/{}/resume", segment(session_id)?);
        self.send_empty(
            self.request(Method::POST, &path).json(&json!({})),
            "session resume",
        )
        .await
    }

    /// Forks `count` independent children from a checkpoint, or from the
    /// session's state now when `from_checkpoint` is `None` (the runtime
    /// checkpoints first). All or nothing: on failure no child is left.
    pub async fn fork_session(
        &self,
        session_id: &str,
        from_checkpoint: Option<&str>,
        count: u8,
    ) -> Result<Vec<Session>, DoError> {
        if !(1..=FORK_MAX_COUNT).contains(&count) {
            return Err(DoError::Invalid(format!(
                "a fork creates 1 to {FORK_MAX_COUNT} sessions, not {count}"
            )));
        }
        let path = format!("{PREFIX}/sessions/{}/fork", segment(session_id)?);
        let mut body = json!({ "count": count });
        if let Some(checkpoint) = from_checkpoint {
            body["from_checkpoint_id"] = json!(segment(checkpoint)?);
        }
        let req = self
            .request(Method::POST, &path)
            .timeout(SNAPSHOT_TIMEOUT)
            .json(&body);
        let page: SessionsPage = self.send(req, "session fork").await?;
        Ok(page.sessions)
    }

    /// Runs one command in the sandbox and waits for it.
    pub async fn exec(&self, session_id: &str, exec: &ExecRequest) -> Result<ExecOutput, DoError> {
        if exec.argv.is_empty() {
            return Err(DoError::Invalid("an exec needs a command".into()));
        }
        let path = format!("{PREFIX}/sessions/{}/sandbox/exec", segment(session_id)?);
        let budget = match exec.timeout_seconds {
            Some(secs) if secs > 0 => Duration::from_secs(secs),
            _ => EXEC_DEFAULT,
        };
        let req = self
            .request(Method::POST, &path)
            .timeout(budget + EXEC_MARGIN)
            .json(exec);
        self.send(req, "sandbox exec").await
    }

    /// The selectable sandbox sizes, smallest first.
    pub async fn sizes(&self) -> Result<Vec<Size>, DoError> {
        let path = format!("{PREFIX}/sessions/sandbox/sizes");
        let root: SizesRoot = self
            .send(self.request(Method::GET, &path), "size list")
            .await?;
        Ok(root.sizes)
    }

    // ---- agent configs ----

    pub async fn list_configs(&self) -> Result<Vec<ConfigSummary>, DoError> {
        self.list_all::<ConfigsPage>(&format!("{PREFIX}/configs"), Vec::new(), "config list")
            .await
    }

    pub async fn get_config(&self, config_id: &str) -> Result<Config, DoError> {
        const WHAT: &str = "config get";
        let path = format!("{PREFIX}/configs/{}", segment(config_id)?);
        let root: ConfigRoot = self.send(self.request(Method::GET, &path), WHAT).await?;
        root.config.ok_or_else(|| missing(WHAT, "config"))
    }

    /// Stores a manifest as a reusable config, for sessions created with
    /// `create_session_from_config`.
    pub async fn create_config(&self, name: &str, manifest: &Manifest) -> Result<Config, DoError> {
        const WHAT: &str = "config create";
        if name.trim().is_empty() {
            return Err(DoError::Invalid("a config needs a name".into()));
        }
        let manifest_yaml =
            serde_json::to_string(manifest).map_err(|e| DoError::Invalid(e.to_string()))?;
        let req = self
            .request(Method::POST, &format!("{PREFIX}/configs"))
            .json(&json!({ "name": name, "manifest_yaml": manifest_yaml }));
        let root: ConfigRoot = self.send(req, WHAT).await?;
        root.config.ok_or_else(|| missing(WHAT, "config"))
    }

    /// Deletes a config. One already gone is not an error.
    pub async fn delete_config(&self, config_id: &str) -> Result<(), DoError> {
        let path = format!("{PREFIX}/configs/{}", segment(config_id)?);
        self.send_idempotent_delete(self.request(Method::DELETE, &path), "config delete")
            .await
    }

    /// The sessions started from one config.
    pub async fn list_config_sessions(&self, config_id: &str) -> Result<Vec<Session>, DoError> {
        let path = format!("{PREFIX}/configs/{}/sessions", segment(config_id)?);
        self.list_all::<SessionsPage>(&path, Vec::new(), "config session list")
            .await
    }

    // ---- checkpoints ----

    /// Takes a checkpoint and waits until it is READY. The runtime allows
    /// one only between agent turns, which a bare sandbox always is.
    pub async fn create_checkpoint(
        &self,
        session_id: &str,
        label: Option<&str>,
    ) -> Result<Checkpoint, DoError> {
        const WHAT: &str = "checkpoint create";
        let path = format!("{PREFIX}/sessions/{}/checkpoints", segment(session_id)?);
        let body = match label {
            Some(label) => json!({ "label": label }),
            None => json!({}),
        };
        let req = self
            .request(Method::POST, &path)
            .timeout(SNAPSHOT_TIMEOUT)
            .json(&body);
        let root: CheckpointRoot = self.send(req, WHAT).await?;
        root.checkpoint.ok_or_else(|| missing(WHAT, "checkpoint"))
    }

    /// A session's checkpoints, newest first as the API lists them.
    pub async fn list_checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, DoError> {
        let path = format!("{PREFIX}/sessions/{}/checkpoints", segment(session_id)?);
        self.list_all::<CheckpointsPage>(&path, Vec::new(), "checkpoint list")
            .await
    }

    pub async fn get_checkpoint(
        &self,
        session_id: &str,
        checkpoint_id: &str,
    ) -> Result<Checkpoint, DoError> {
        const WHAT: &str = "checkpoint get";
        let path = format!(
            "{PREFIX}/sessions/{}/checkpoints/{}",
            segment(session_id)?,
            segment(checkpoint_id)?
        );
        let root: CheckpointRoot = self.send(self.request(Method::GET, &path), WHAT).await?;
        root.checkpoint.ok_or_else(|| missing(WHAT, "checkpoint"))
    }

    /// Deletes a checkpoint and its capture. Idempotent on the API's side.
    pub async fn delete_checkpoint(
        &self,
        session_id: &str,
        checkpoint_id: &str,
    ) -> Result<(), DoError> {
        let path = format!(
            "{PREFIX}/sessions/{}/checkpoints/{}",
            segment(session_id)?,
            segment(checkpoint_id)?
        );
        self.send_idempotent_delete(self.request(Method::DELETE, &path), "checkpoint delete")
            .await
    }

    // ---- custom templates ----

    pub async fn list_templates(&self) -> Result<Vec<Template>, DoError> {
        self.list_all::<TemplatesPage>(&format!("{PREFIX}/templates"), Vec::new(), "template list")
            .await
    }

    /// One template, by id or name; its `status` is the build's progress.
    pub async fn get_template(&self, template: &str) -> Result<Template, DoError> {
        const WHAT: &str = "template get";
        let path = format!("{PREFIX}/templates/{}", segment(template)?);
        let root: TemplateRoot = self.send(self.request(Method::GET, &path), WHAT).await?;
        root.template.ok_or_else(|| missing(WHAT, "template"))
    }

    /// Registers an image as a template and starts its build: a rebase onto
    /// `base_template` that took about seven minutes when tried, ending in an
    /// image whose digest is the runtime's, not the source's.
    pub async fn create_template(
        &self,
        name: &str,
        base_template: &str,
        source_oci_ref: &str,
    ) -> Result<Template, DoError> {
        const WHAT: &str = "template create";
        if [name, base_template, source_oci_ref]
            .iter()
            .any(|v| v.trim().is_empty())
        {
            return Err(DoError::Invalid(
                "a template needs a name, a base template and a source image".into(),
            ));
        }
        let req = self
            .request(Method::POST, &format!("{PREFIX}/templates"))
            .json(&json!({
                "name": name,
                "base_template": base_template,
                "source_oci_ref": source_oci_ref,
            }));
        let root: TemplateRoot = self.send(req, WHAT).await?;
        root.template.ok_or_else(|| missing(WHAT, "template"))
    }

    /// Points a template at a new source image or base, which starts a new
    /// build.
    pub async fn update_template(
        &self,
        template: &str,
        base_template: Option<&str>,
        source_oci_ref: Option<&str>,
    ) -> Result<Template, DoError> {
        const WHAT: &str = "template update";
        let path = format!("{PREFIX}/templates/{}", segment(template)?);
        let mut body = serde_json::Map::new();
        if let Some(base) = base_template {
            body.insert("base_template".into(), json!(base));
        }
        if let Some(source) = source_oci_ref {
            body.insert("source_oci_ref".into(), json!(source));
        }
        if body.is_empty() {
            return Err(DoError::Invalid(
                "a template update needs a base template or a source image".into(),
            ));
        }
        let req = self.request(Method::PUT, &path).json(&body);
        let root: TemplateRoot = self.send(req, WHAT).await?;
        root.template.ok_or_else(|| missing(WHAT, "template"))
    }

    /// Deletes a template. One already gone is not an error.
    pub async fn delete_template(&self, template: &str) -> Result<(), DoError> {
        let path = format!("{PREFIX}/templates/{}", segment(template)?);
        self.send_idempotent_delete(self.request(Method::DELETE, &path), "template delete")
            .await
    }

    pub async fn list_template_builds(
        &self,
        template: &str,
    ) -> Result<Vec<TemplateBuild>, DoError> {
        let path = format!("{PREFIX}/templates/{}/builds", segment(template)?);
        self.list_all::<BuildsPage>(&path, Vec::new(), "template build list")
            .await
    }

    pub async fn get_template_build(
        &self,
        template: &str,
        build_id: &str,
    ) -> Result<TemplateBuild, DoError> {
        const WHAT: &str = "template build get";
        let path = format!(
            "{PREFIX}/templates/{}/builds/{}",
            segment(template)?,
            segment(build_id)?
        );
        let root: BuildRoot = self.send(self.request(Method::GET, &path), WHAT).await?;
        root.build.ok_or_else(|| missing(WHAT, "build"))
    }

    // ---- account ----

    /// The team's prepayment balance. Not part of the Harness Runtime API,
    /// but what decides whether its sessions keep running.
    pub async fn balance(&self) -> Result<Balance, DoError> {
        const WHAT: &str = "balance";
        let root: BalanceRoot = self
            .send(
                self.request(Method::GET, "/v2/customers/my/prepayment_config"),
                WHAT,
            )
            .await?;
        root.status.ok_or_else(|| missing(WHAT, "status"))
    }

    // ---- plumbing ----

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
    }

    async fn send<T: DeserializeOwned>(
        &self,
        req: RequestBuilder,
        what: &'static str,
    ) -> Result<T, DoError> {
        let response = req
            .send()
            .await
            .map_err(|source| DoError::Transport { what, source })?;
        read(response, what).await
    }

    async fn send_empty(&self, req: RequestBuilder, what: &'static str) -> Result<(), DoError> {
        let response = req
            .send()
            .await
            .map_err(|source| DoError::Transport { what, source })?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|source| DoError::Transport { what, source })?;
        if status.is_success() {
            Ok(())
        } else {
            Err(api_error(what, status, &body))
        }
    }

    async fn send_idempotent_delete(
        &self,
        req: RequestBuilder,
        what: &'static str,
    ) -> Result<(), DoError> {
        match self.send_empty(req, what).await {
            Err(error) if error.is_not_found() => Ok(()),
            other => other,
        }
    }

    async fn list_all<P>(
        &self,
        path: &str,
        query: Vec<(&'static str, String)>,
        what: &'static str,
    ) -> Result<Vec<P::Item>, DoError>
    where
        P: Page + DeserializeOwned,
    {
        let mut items = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut q = query.clone();
            if let Some(token) = &token {
                q.push(("page_token", token.clone()));
            }
            let page: P = self
                .send(self.request(Method::GET, path).query(&q), what)
                .await?;
            let (mut page_items, next) = page.split();
            items.append(&mut page_items);
            match next {
                Some(next) if Some(&next) != token.as_ref() => token = Some(next),
                Some(_) => {
                    return Err(DoError::Decode {
                        what,
                        detail: "the API repeated its page token".into(),
                    })
                }
                None => return Ok(items),
            }
        }
        Err(DoError::Decode {
            what,
            detail: format!("more than {MAX_PAGES} pages"),
        })
    }
}

async fn read<T: DeserializeOwned>(
    response: reqwest::Response,
    what: &'static str,
) -> Result<T, DoError> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|source| DoError::Transport { what, source })?;
    if !status.is_success() {
        return Err(api_error(what, status, &body));
    }
    serde_json::from_slice(&body).map_err(|e| DoError::Decode {
        what,
        detail: e.to_string(),
    })
}

fn api_error(what: &'static str, status: StatusCode, body: &[u8]) -> DoError {
    let parsed: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
    let listed = parsed.errors.into_iter().next();
    let message = parsed
        .message
        .filter(|m| !m.is_empty())
        .or_else(|| parsed.error.and_then(|e| e.message))
        .or_else(|| listed.as_ref().and_then(|e| e.detail.clone()))
        .unwrap_or_else(|| {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            if text.is_empty() {
                status
                    .canonical_reason()
                    .unwrap_or("no reason given")
                    .to_string()
            } else {
                text.chars().take(500).collect()
            }
        });
    DoError::Api {
        what,
        status: status.as_u16(),
        id: parsed.id,
        message,
        request_id: parsed
            .request_id
            .or_else(|| listed.and_then(|e| e.request_id)),
    }
}

fn missing(what: &'static str, field: &str) -> DoError {
    DoError::Decode {
        what,
        detail: format!("the response has no {field}"),
    }
}

/// An id or name placed in a path: only what the API's ids and slugs are
/// made of, so nothing can step to another route.
fn segment(value: &str) -> Result<&str, DoError> {
    let ok = !value.is_empty()
        && value.len() <= 200
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(value)
    } else {
        Err(DoError::Invalid(format!("not an id or name: {value:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_refuse_anything_that_could_change_the_route() {
        assert!(segment("0190c0de-0000-7000-8000-000000000001").is_ok());
        assert!(segment("tracon-hdr.v2_x").is_ok());
        for bad in ["", ".", "..", "a/b", "a?b", "a#b", "a b", "%2e%2e"] {
            assert!(segment(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn unknown_field_reads_the_api_refusal() {
        let error = api_error(
            "session create",
            StatusCode::BAD_REQUEST,
            br#"{"id":"bad_request","message":"agentspec: unknown field \"region\" (unknown or misspelled fields are rejected)","request_id":"r"}"#,
        );
        assert_eq!(error.unknown_field(), Some("region"));
        assert!(!error.is_retryable());
    }

    #[test]
    fn every_error_envelope_yields_its_message() {
        let nested = api_error(
            "x",
            StatusCode::CONFLICT,
            br#"{"error":{"code":409,"message":"session is destroyed"}}"#,
        );
        assert!(nested.to_string().contains("session is destroyed"));
        let listed = api_error(
            "x",
            StatusCode::NOT_FOUND,
            br#"{"errors":[{"detail":"session not found","status":404,"request_id":"q"}]}"#,
        );
        assert!(listed.is_not_found());
        assert!(listed.to_string().contains("session not found"));
        let bare = api_error("x", StatusCode::BAD_GATEWAY, b"");
        assert!(bare.to_string().contains("Bad Gateway"));
        assert!(bare.is_retryable());
    }

    #[test]
    fn regional_hosts_are_built_from_a_slug_only() {
        let client = Client::regional("MEM1", "t").unwrap();
        assert_eq!(client.base_url(), "https://api-mem1.digitalocean.com");
        assert_eq!(
            Client::regional("", "t").unwrap().base_url(),
            DEFAULT_BASE_URL
        );
        assert!(Client::regional("evil.com/", "t").is_err());
        assert!(Client::new(" ").is_err());
    }
}
