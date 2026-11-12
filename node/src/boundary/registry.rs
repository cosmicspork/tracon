//! What a registry holds, asked over the OCI distribution API: whether an
//! image is there, what digest a tag names, and removing one. Runtime-neutral:
//! it knows image references and a Docker `config.json`, nothing about where
//! the images run.
//!
//! Authentication is the registry's own challenge: a request without
//! credentials is answered 401 with `WWW-Authenticate`, which names either a
//! token service (`Bearer realm=…,service=…,scope=…`) or `Basic`. The token
//! service is given the Docker config's credential for the registry's host,
//! or nothing, which public registries answer with an anonymous pull token.
//!
//! DigitalOcean's registry routes a manifest `DELETE` and asks for the
//! `delete` action on the repository (observed 2026-10-13: an unauthenticated
//! `DELETE /v2/<repo>/manifests/<digest>` is answered 401 with
//! `scope="repository:<repo>:delete"`). Whether its token service grants that
//! action to a read/write registry credential is not documented, and its own
//! documented route is the API's
//! `DELETE /v2/registries/{registry}/repositories/{repo}/digests/{digest}`.
//! So a removal tries the distribution API first and, on that host only,
//! falls back to the DigitalOcean API when the registry refuses it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use reqwest::header::{ACCEPT, WWW_AUTHENTICATE};
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};

/// Every manifest shape an image may be pushed as: an index (a multi-platform
/// image, or one carrying attestations) and a single manifest, in the OCI and
/// the Docker spelling. A registry asked without one of these may answer with
/// a converted manifest whose digest is not the pushed one.
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json";

pub const DIGITALOCEAN_HOST: &str = "registry.digitalocean.com";
const DIGITALOCEAN_API: &str = "https://api.digitalocean.com";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RegistryError(pub String);

impl From<reqwest::Error> for RegistryError {
    fn from(error: reqwest::Error) -> Self {
        Self(error.to_string())
    }
}

/// An image reference taken apart the way a registry is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// As written, without its tag or digest: what a digest is appended to.
    pub name: String,
    /// The host the API is called on: `registry-1.docker.io` for Docker Hub.
    pub host: String,
    pub repository: String,
    /// A tag, or `sha256:<hex>`.
    pub reference: String,
}

impl Reference {
    pub fn parse(image: &str) -> Result<Self, RegistryError> {
        let bad = |why: &str| RegistryError(format!("not an image reference ({why}): {image}"));
        let (rest, digest) = match image.split_once('@') {
            Some((rest, digest)) => (rest, Some(digest)),
            None => (image, None),
        };
        // A tag is the last `:` after the last `/`; a registry's port is not.
        let (name, tag) = match rest.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => (name, Some(tag)),
            _ => (rest, None),
        };
        let (host, path) = match name.split_once('/') {
            Some((first, path)) if first.contains(['.', ':']) || first == "localhost" => {
                (first.to_string(), path.to_string())
            }
            _ => ("docker.io".to_string(), name.to_string()),
        };
        let repository = match host == "docker.io" && !path.contains('/') {
            true => format!("library/{path}"),
            false => path,
        };
        let host = match host.as_str() {
            "docker.io" | "index.docker.io" => "registry-1.docker.io".to_string(),
            _ => host,
        };
        let valid_repository = !repository.is_empty()
            && repository.split('/').all(|part| {
                let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
                part.as_bytes().first().is_some_and(alnum)
                    && part.as_bytes().last().is_some_and(alnum)
                    && part.bytes().all(|b| alnum(&b) || b"._-".contains(&b))
            });
        if !valid_repository {
            return Err(bad("repository"));
        }
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
        {
            return Err(bad("host"));
        }
        let reference = match (digest, tag) {
            (Some(digest), _) => {
                crate::config::immutable_image(&format!("x@{digest}"))
                    .map_err(|_| bad("digest"))?;
                digest.to_string()
            }
            (None, Some(tag)) => {
                let valid = !tag.is_empty()
                    && tag.len() <= 128
                    && tag
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
                if !valid {
                    return Err(bad("tag"));
                }
                tag.to_string()
            }
            (None, None) => "latest".to_string(),
        };
        Ok(Self {
            name: name.to_string(),
            host,
            repository,
            reference,
        })
    }

    pub fn is_digest(&self) -> bool {
        self.reference.starts_with("sha256:")
    }
}

#[derive(serde::Deserialize, Default)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,
}

#[derive(serde::Deserialize, Default)]
struct AuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

/// The user and password a Docker `config.json` in `dir` holds for `host`.
/// Credential helpers are not consulted: a node in a pod has none.
///
/// A key spelled exactly as the host wins. Otherwise the keys that name the
/// host some other way (`https://host`, `host/v1/`, Docker Hub's aliases)
/// must be one: two would make which credential is presented depend on map
/// order, so that is refused.
pub fn credentials(dir: &Path, host: &str) -> Result<Option<(String, String)>, RegistryError> {
    let path = dir.join("config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RegistryError(format!("{}: {error}", path.display()))),
    };
    credentials_in(&text, &path, host)
}

/// `credentials`, from a `config.json` already read.
fn credentials_in(
    text: &str,
    path: &Path,
    host: &str,
) -> Result<Option<(String, String)>, RegistryError> {
    let config: DockerConfig = serde_json::from_str(text)
        .map_err(|error| RegistryError(format!("{}: {error}", path.display())))?;
    let hub = ["registry-1.docker.io", "index.docker.io", "docker.io"];
    let names_host = |key: &str| {
        let key = key
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let key = key.split('/').next().unwrap_or(key);
        key == host || (hub.contains(&host) && hub.contains(&key))
    };
    let entry = match config.auths.get(host) {
        Some(entry) => entry,
        None => {
            let mut matching: Vec<_> = config
                .auths
                .iter()
                .filter(|(key, _)| names_host(key))
                .collect();
            matching.sort_by(|a, b| a.0.cmp(b.0));
            match matching.as_slice() {
                [] => return Ok(None),
                [(_, entry)] => *entry,
                many => {
                    let keys: Vec<&str> = many.iter().map(|(key, _)| key.as_str()).collect();
                    return Err(RegistryError(format!(
                        "{} holds more than one credential for {host} ({}); keep one",
                        path.display(),
                        keys.join(", ")
                    )));
                }
            }
        }
    };
    if let (Some(user), Some(password)) = (&entry.username, &entry.password) {
        return Ok(Some((user.clone(), password.clone())));
    }
    let malformed = || {
        RegistryError(format!(
            "the credential for {host} in {} is not a base64 user:password",
            path.display()
        ))
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(entry.auth.as_deref().ok_or_else(malformed)?.trim())
        .map_err(|_| malformed())?;
    let decoded = String::from_utf8(decoded).map_err(|_| malformed())?;
    let (user, password) = decoded.split_once(':').ok_or_else(malformed)?;
    Ok(Some((user.to_string(), password.to_string())))
}

/// The actions a registry token grants on `repository`, when the token is a
/// JWT that names it in its `access` claim, as Docker's token specification
/// has it. `None` whenever the token does not say: an opaque token, a JWT
/// without the claim, or one whose claim names the repository some other way.
/// Only a token that names the repository and leaves push out is a refusal.
fn granted_actions(token: &str, repository: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = token.split('.').collect();
    let [_, payload, _] = parts.as_slice() else {
        return None;
    };
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    let access = claims.get("access")?.as_array()?;
    let named: Vec<&serde_json::Value> = access
        .iter()
        .filter(|entry| entry["type"] == "repository" && entry["name"] == repository)
        .collect();
    if named.is_empty() {
        return None;
    }
    Some(
        named
            .into_iter()
            .flat_map(|entry| entry["actions"].as_array().cloned().unwrap_or_default())
            .filter_map(|action| action.as_str().map(str::to_string))
            .collect(),
    )
}

/// What a 401 asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Challenge {
    Bearer {
        realm: String,
        service: Option<String>,
        scope: Option<String>,
    },
    Basic,
}

/// `Bearer realm="…",service="…",scope="…"`. A scope may itself hold commas
/// (`repository:a:pull,push`), so values are read as quoted strings.
fn parse_challenge(header: &str) -> Option<Challenge> {
    let (scheme, params) = header.trim().split_once(' ').unwrap_or((header.trim(), ""));
    if scheme.eq_ignore_ascii_case("basic") {
        return Some(Challenge::Basic);
    }
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut values = HashMap::new();
    let mut rest = params.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let key = key
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        let after = after.trim_start();
        let (value, remaining) = match after.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted.find('"')?;
                (&quoted[..end], &quoted[end + 1..])
            }
            None => match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            },
        };
        values.insert(key, value.to_string());
        rest = remaining.trim_start().trim_start_matches(',').trim_start();
    }
    Some(Challenge::Bearer {
        realm: values.remove("realm")?,
        service: values.remove("service"),
        scope: values.remove("scope"),
    })
}

enum Auth {
    Bearer(String),
    Basic(String, String),
}

/// The client. Cheap to clone; reads the Docker config on every call, so a
/// rotated credential is picked up without a restart.
#[derive(Clone)]
pub struct Registry {
    http: reqwest::Client,
    docker_config: Option<PathBuf>,
    scheme: &'static str,
    digitalocean_host: String,
    digitalocean_api: String,
    digitalocean_token: Option<PathBuf>,
}

impl Registry {
    pub fn new(docker_config: Option<PathBuf>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            http,
            docker_config,
            scheme: "https",
            digitalocean_host: DIGITALOCEAN_HOST.into(),
            digitalocean_api: DIGITALOCEAN_API.into(),
            digitalocean_token: None,
        }
    }

    /// The token the DigitalOcean API fallback presents, instead of the
    /// registry credential's password.
    pub fn with_digitalocean_token(mut self, token_file: Option<PathBuf>) -> Self {
        self.digitalocean_token = token_file;
        self
    }

    /// A test's fake registry: plain HTTP, and its own stand-in for
    /// DigitalOcean's host and API.
    #[doc(hidden)]
    pub fn for_test(mut self, digitalocean_host: &str, digitalocean_api: &str) -> Self {
        self.scheme = "http";
        self.digitalocean_host = digitalocean_host.into();
        self.digitalocean_api = digitalocean_api.into();
        self
    }

    fn credentials(&self, host: &str) -> Result<Option<(String, String)>, RegistryError> {
        match self.docker_config.as_deref() {
            // Read through the path guard: the directory checked, and the
            // file compared with what the check found once it is open.
            Some(dir) => match crate::config::path_guard::read_secret_in(
                "runtime.kubernetes.builder.docker_config",
                dir,
                "config.json",
            )
            .map_err(|error| RegistryError(error.to_string()))?
            {
                Some(text) => credentials_in(&text, &dir.join("config.json"), host),
                None => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// One request, answering the registry's challenge if it makes one.
    async fn send(
        &self,
        method: Method,
        host: &str,
        path: &str,
        scope: Option<String>,
    ) -> Result<reqwest::Response, RegistryError> {
        let url = format!("{}://{host}/v2/{path}", self.scheme);
        let request = |auth: Option<&Auth>| {
            let mut request = self
                .http
                .request(method.clone(), &url)
                .header(ACCEPT, MANIFEST_TYPES);
            match auth {
                Some(Auth::Bearer(token)) => request = request.bearer_auth(token),
                Some(Auth::Basic(user, password)) => {
                    request = request.basic_auth(user, Some(password))
                }
                None => {}
            }
            request.send()
        };
        let first = request(None).await?;
        if first.status() != StatusCode::UNAUTHORIZED {
            return Ok(first);
        }
        let challenge = first
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_challenge)
            .ok_or_else(|| {
                RegistryError(format!(
                    "{host} refused the request and named no way to authenticate"
                ))
            })?;
        let credential = self.credentials(host)?;
        let auth = match challenge {
            Challenge::Basic => {
                let (user, password) = credential.ok_or_else(|| {
                    RegistryError(format!(
                        "{host} asks for a credential and the Docker config holds none for it"
                    ))
                })?;
                Auth::Basic(user, password)
            }
            Challenge::Bearer {
                realm,
                service,
                scope: offered,
            } => Auth::Bearer(
                self.token(
                    &realm,
                    service.as_deref(),
                    offered.or(scope).as_deref(),
                    credential,
                )
                .await?,
            ),
        };
        Ok(request(Some(&auth)).await?)
    }

    async fn token(
        &self,
        realm: &str,
        service: Option<&str>,
        scope: Option<&str>,
        credential: Option<(String, String)>,
    ) -> Result<String, RegistryError> {
        self.token_or_refusal(realm, service, scope, credential)
            .await
            .map_err(|refused| refused.error)
    }

    /// `token`, saying whether a refusal was the registry's repository limit.
    async fn token_or_refusal(
        &self,
        realm: &str,
        service: Option<&str>,
        scope: Option<&str>,
        credential: Option<(String, String)>,
    ) -> Result<String, TokenRefused> {
        if !realm.starts_with("https://")
            && !(self.scheme == "http" && realm.starts_with("http://"))
        {
            return Err(RegistryError(format!(
                "refusing a token service that is not HTTPS: {realm}"
            ))
            .into());
        }
        let mut query = Vec::new();
        if let Some(service) = service {
            query.push(("service", service));
        }
        if let Some(scope) = scope {
            query.push(("scope", scope));
        }
        let mut request = self.http.get(realm).query(&query);
        if let Some((user, password)) = &credential {
            request = request.basic_auth(user, Some(password));
        }
        let response = request.send().await.map_err(RegistryError::from)?;
        let status = response.status();
        if !status.is_success() {
            let said = refusal_message(&response.text().await.unwrap_or_default());
            let at_repository_limit = status == StatusCode::FORBIDDEN
                && said.as_deref().is_some_and(names_a_repository_limit);
            return Err(TokenRefused {
                error: RegistryError(format!(
                    "the token service {realm} answered {status}{}{}",
                    match credential {
                        Some(_) => "",
                        None => " (the Docker config holds no credential for this registry)",
                    },
                    said.map(|said| format!(": {said}")).unwrap_or_default()
                )),
                at_repository_limit,
            });
        }
        #[derive(serde::Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let token: Token = response.json().await.map_err(RegistryError::from)?;
        token
            .token
            .or(token.access_token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                RegistryError(format!("the token service {realm} returned no token")).into()
            })
    }

    /// The digest `image` names in its registry, or `None` where the registry
    /// holds nothing under it.
    pub async fn digest(&self, image: &str) -> Result<Option<String>, RegistryError> {
        let image = Reference::parse(image)?;
        let path = format!("{}/manifests/{}", image.repository, image.reference);
        let pull = Some(format!("repository:{}:pull", image.repository));
        let response = self
            .send(Method::HEAD, &image.host, &path, pull.clone())
            .await?;
        match response.status() {
            StatusCode::NOT_FOUND => return Ok(None),
            status if !status.is_success() => {
                return Err(RegistryError(format!(
                    "{} answered {status} for {}",
                    image.host, image.name
                )))
            }
            _ => {}
        }
        let header = response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let digest = match (header, image.is_digest()) {
            (Some(digest), _) => digest,
            (None, true) => image.reference.clone(),
            // A registry that leaves the header off a HEAD: the digest is the
            // manifest's own bytes, as served.
            (None, false) => {
                let body = self
                    .send(Method::GET, &image.host, &path, pull)
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;
                format!("sha256:{}", hex::encode(Sha256::digest(&body)))
            }
        };
        crate::config::immutable_image(&format!("x@{digest}")).map_err(|_| {
            RegistryError(format!(
                "{} named no sha256 digest for {}",
                image.host, image.name
            ))
        })?;
        Ok(Some(digest))
    }

    pub async fn exists(&self, image: &str) -> bool {
        self.presence(image).await == crate::boundary::Presence::Present
    }

    /// Whether the registry holds `image`, or `Unknown` when it could not be
    /// asked: a registry that is down is not a registry that pruned it.
    pub async fn presence(&self, image: &str) -> crate::boundary::Presence {
        use crate::boundary::Presence;
        if Reference::parse(image).is_err() {
            return Presence::Absent;
        }
        match self.digest(image).await {
            Ok(Some(_)) => Presence::Present,
            Ok(None) => Presence::Absent,
            Err(error) => {
                tracing::warn!(%image, %error, "could not ask the registry for an image");
                Presence::Unknown
            }
        }
    }

    /// `name@sha256:…` for what `image` names now.
    pub async fn identity(&self, image: &str) -> Option<String> {
        let digest = self.digest(image).await.ok()??;
        let name = Reference::parse(image).ok()?.name;
        Some(format!("{name}@{digest}"))
    }

    /// `name@sha256:…` of the manifest `image` has for Linux on `arch`: the
    /// entry an index or manifest list names for it, or the manifest itself
    /// when `image` is a single platform's. `None` where the registry holds
    /// nothing under `image`.
    pub async fn platform_identity(
        &self,
        image: &str,
        arch: &str,
    ) -> Result<Option<String>, RegistryError> {
        let reference = Reference::parse(image)?;
        let path = format!("{}/manifests/{}", reference.repository, reference.reference);
        let pull = Some(format!("repository:{}:pull", reference.repository));
        let response = self.send(Method::GET, &reference.host, &path, pull).await?;
        match response.status() {
            StatusCode::NOT_FOUND => return Ok(None),
            status if !status.is_success() => {
                return Err(RegistryError(format!(
                    "{} answered {status} for {}",
                    reference.host, reference.name
                )))
            }
            _ => {}
        }
        let body = response.bytes().await?;
        let digest = match platform_entry(&body, arch)? {
            Some(digest) => digest,
            // The digest is the manifest's own bytes, as served, whatever a
            // header claims.
            None => format!("sha256:{}", hex::encode(Sha256::digest(&body))),
        };
        Ok(Some(format!("{}@{digest}", reference.name)))
    }

    /// Remove the manifest `image` names, and with it every tag on it. One
    /// the registry no longer holds is already removed.
    pub async fn remove(&self, image: &str) -> Result<(), RegistryError> {
        let reference = Reference::parse(image)?;
        let digest = match reference.is_digest() {
            true => reference.reference.clone(),
            false => match self.digest(image).await? {
                Some(digest) => digest,
                None => return Ok(()),
            },
        };
        let path = format!("{}/manifests/{digest}", reference.repository);
        let delete = Some(format!("repository:{}:delete", reference.repository));
        let refused = match self
            .send(Method::DELETE, &reference.host, &path, delete)
            .await
        {
            Ok(response) => {
                let status = response.status();
                if status.is_success() || status == StatusCode::NOT_FOUND {
                    return Ok(());
                }
                let body = response.text().await.unwrap_or_default();
                let code = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| value["errors"][0]["code"].as_str().map(str::to_string))
                    .unwrap_or_default();
                format!(
                    "{} answered {status}{} to deleting {}@{digest}",
                    reference.host,
                    if code.is_empty() {
                        String::new()
                    } else {
                        format!(" ({code})")
                    },
                    reference.name
                )
            }
            Err(error) => error.0,
        };
        if reference.host != self.digitalocean_host {
            return Err(RegistryError(refused));
        }
        self.remove_through_digitalocean(&reference, &digest)
            .await
            .map_err(|error| {
                RegistryError(format!("{refused}; through the DigitalOcean API: {error}"))
            })
    }

    async fn remove_through_digitalocean(
        &self,
        reference: &Reference,
        digest: &str,
    ) -> Result<(), RegistryError> {
        let (registry, repository) = reference
            .repository
            .split_once('/')
            .ok_or_else(|| RegistryError(format!("{} names no registry", reference.name)))?;
        let token = match &self.digitalocean_token {
            Some(file) => crate::config::path_guard::read_secret(
                "runtime.kubernetes.builder.do_token_file",
                file,
            )
            .map(|token| token.trim().to_string())
            .map_err(|error| RegistryError(format!("{}: {error}", file.display())))?,
            None => {
                self.credentials(&reference.host)?
                    .ok_or_else(|| RegistryError("no API token and no registry credential".into()))?
                    .1
            }
        };
        let url = format!(
            "{}/v2/registries/{registry}/repositories/{}/digests/{digest}",
            self.digitalocean_api,
            repository.replace('/', "%2F")
        );
        let response = self.http.delete(&url).bearer_auth(token).send().await?;
        match response.status() {
            status if status.is_success() || status == StatusCode::NOT_FOUND => Ok(()),
            status => Err(RegistryError(format!("answered {status}"))),
        }
    }

    /// Whether the Docker config's credential may push under `prefix`
    /// (`host/namespace`). The registry's token service is asked for push on
    /// a repository there, the token is presented to `/v2/`, and a token that
    /// names the probe repository must grant push on it: a read-only
    /// credential would otherwise pass every check and fail every build. Only
    /// an explicit refusal fails, because the check gates every build: a
    /// token service that refuses the credential, or a token that names the
    /// repository without push. `Ok(false)` where the credential is accepted
    /// but the token does not say (an opaque token, a JWT without an `access`
    /// claim or naming the repository otherwise, or a Basic registry).
    ///
    /// The probe repository does not exist, so a registry at its repository
    /// limit (DOCR's plans cap them) refuses it although every repository
    /// already there takes pushes: that is [`PushCheck::NoNewRepositories`],
    /// not a refusal, or a full registry would stop every build.
    pub async fn check_push(&self, prefix: &str) -> Result<PushCheck, RegistryError> {
        let host = prefix.split('/').next().unwrap_or(prefix);
        let credential = self.credentials(host)?.ok_or_else(|| {
            RegistryError(format!(
                "the Docker config{} holds no credential for {host}",
                self.docker_config
                    .as_ref()
                    .map(|dir| format!(" in {}", dir.display()))
                    .unwrap_or_default()
            ))
        })?;
        let url = format!("{}://{host}/v2/", self.scheme);
        let first = self.http.get(&url).send().await?;
        match first.status() {
            StatusCode::UNAUTHORIZED => {}
            status if status.is_success() => return Ok(PushCheck::Unsaid),
            status => return Err(RegistryError(format!("{host} answered {status}"))),
        }
        let challenge = first
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_challenge)
            .ok_or_else(|| RegistryError(format!("{host} named no way to authenticate")))?;
        let refused = |status: StatusCode| {
            RegistryError(format!("{host} answered {status} with the credential"))
        };
        match challenge {
            Challenge::Basic => {
                let (user, password) = credential;
                let response = self
                    .http
                    .get(&url)
                    .basic_auth(user, Some(password))
                    .send()
                    .await?;
                match response.status() {
                    status if status.is_success() => Ok(PushCheck::Unsaid),
                    status => Err(refused(status)),
                }
            }
            Challenge::Bearer { realm, service, .. } => {
                let namespace = prefix.split_once('/').map(|(_, rest)| rest).unwrap_or("");
                let repository = match namespace.is_empty() {
                    true => "tracon-repo-probe".to_string(),
                    false => format!("{namespace}/tracon-repo-probe"),
                };
                let scope = format!("repository:{repository}:push,pull");
                let token = match self
                    .token_or_refusal(&realm, service.as_deref(), Some(&scope), Some(credential))
                    .await
                {
                    Ok(token) => token,
                    Err(refused) if refused.at_repository_limit => {
                        return Ok(PushCheck::NoNewRepositories(refused.error.0))
                    }
                    Err(refused) => return Err(refused.error),
                };
                let response = self.http.get(&url).bearer_auth(&token).send().await?;
                if !response.status().is_success() {
                    return Err(refused(response.status()));
                }
                match granted_actions(&token, &repository) {
                    Some(actions) if !actions.iter().any(|action| action == "push") => {
                        Err(RegistryError(format!(
                            "the credential may not push to {prefix} (the token grants {})",
                            match actions.is_empty() {
                                true => "nothing on it".to_string(),
                                false => actions.join(", "),
                            }
                        )))
                    }
                    Some(_) => Ok(PushCheck::Granted),
                    None => Ok(PushCheck::Unsaid),
                }
            }
        }
    }
}

/// What [`Registry::check_push`] could tell of the credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushCheck {
    /// The token grants push on the probe repository.
    Granted,
    /// The credential is accepted, but nothing says whether it may push.
    Unsaid,
    /// The registry is at its repository limit: it refuses the probe
    /// repository, as it will any repository not already there, in its
    /// own words.
    NoNewRepositories(String),
}

/// A token request the token service refused, and whether its reason was the
/// registry's repository limit rather than the credential.
struct TokenRefused {
    error: RegistryError,
    at_repository_limit: bool,
}

impl From<RegistryError> for TokenRefused {
    fn from(error: RegistryError) -> Self {
        Self {
            error,
            at_repository_limit: false,
        }
    }
}

/// The message in a registry's error body (`{"errors":[{"code","message"}]}`,
/// the Docker registry API's), `CODE: message`, or the body itself when it is
/// short plain text.
fn refusal_message(body: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Errors {
        errors: Vec<Error>,
    }
    #[derive(serde::Deserialize)]
    struct Error {
        code: Option<String>,
        message: Option<String>,
    }
    if let Ok(parsed) = serde_json::from_str::<Errors>(body) {
        let said: Vec<String> = parsed
            .errors
            .into_iter()
            .filter_map(|error| match (error.code, error.message) {
                (Some(code), Some(message)) => Some(format!("{code}: {message}")),
                (None, Some(message)) => Some(message),
                (Some(code), None) => Some(code),
                (None, None) => None,
            })
            .collect();
        return (!said.is_empty()).then(|| said.join("; "));
    }
    let body = body.trim();
    (!body.is_empty() && body.len() <= 200 && !body.starts_with('<')).then(|| body.to_string())
}

/// Whether a refusal says the registry holds as many repositories as it may
/// (DOCR: "registry contains 5 repositories, limit is 5").
fn names_a_repository_limit(said: &str) -> bool {
    let said = said.to_ascii_lowercase();
    said.contains("repositor") && said.contains("limit")
}

/// The OCI name of this host's architecture, as an index names a platform.
pub fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The digest of the manifest `manifest` names for Linux on `arch`, when it
/// is an index (OCI) or a manifest list (Docker); `None` when it is a single
/// image's manifest, which is its own platform's. An index with no entry for
/// `arch` is refused rather than answered with another platform's image.
pub fn platform_entry(manifest: &[u8], arch: &str) -> Result<Option<String>, RegistryError> {
    #[derive(serde::Deserialize)]
    struct Listed {
        manifests: Option<Vec<Entry>>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        digest: String,
        platform: Option<Platform>,
    }
    #[derive(serde::Deserialize)]
    struct Platform {
        os: String,
        architecture: String,
        variant: Option<String>,
    }
    let listed: Listed = serde_json::from_slice(manifest)
        .map_err(|error| RegistryError(format!("not a manifest: {error}")))?;
    let Some(entries) = listed.manifests else {
        return Ok(None);
    };
    // Attestations sit in the same index under `unknown/unknown`.
    let mut matching: Vec<Entry> = entries
        .into_iter()
        .filter(|entry| {
            entry
                .platform
                .as_ref()
                .is_some_and(|p| p.os == "linux" && p.architecture == arch)
        })
        .collect();
    // arm64 may be listed by variant; v8 is the one every arm64 host runs.
    matching.sort_by_key(|entry| {
        !matches!(
            entry.platform.as_ref().and_then(|p| p.variant.as_deref()),
            None | Some("v8")
        )
    });
    let entry = matching
        .into_iter()
        .next()
        .ok_or_else(|| RegistryError(format!("the index has no linux/{arch} image")))?;
    crate::config::immutable_image(&format!("x@{}", entry.digest))
        .map_err(|_| RegistryError(format!("the index names no sha256 digest for linux/{arch}")))?;
    Ok(Some(entry.digest))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::extract::{Request, State};
    use axum::http::HeaderValue;
    use axum::response::{IntoResponse, Response};
    use std::sync::{Arc, Mutex};

    const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    /// A distribution registry with a token service, enough of one to answer
    /// what the client asks: manifests by tag and digest, and their removal.
    #[derive(Default)]
    pub(crate) struct Fake {
        /// `repository` → tag or digest → digest.
        pub manifests: Mutex<HashMap<String, HashMap<String, String>>>,
        /// The scopes the token service was asked for.
        pub scopes: Mutex<Vec<String>>,
        /// Every request the registry saw, as `METHOD path`.
        pub seen: Mutex<Vec<String>>,
        /// Refuse a manifest delete, as a registry with deletion off does.
        pub refuse_delete: Mutex<bool>,
        /// Leave `Docker-Content-Digest` off a HEAD.
        pub no_digest_header: Mutex<bool>,
        /// Ask for Basic credentials instead of a token.
        pub basic: Mutex<bool>,
        /// Answer every registry request 503.
        pub down: Mutex<bool>,
        /// Issue JWTs granting these actions on the scope's repository,
        /// instead of opaque tokens.
        pub jwt_actions: Mutex<Option<Vec<String>>>,
        /// Name this repository in the JWT's claim instead of the scope's;
        /// an empty name leaves the `access` claim out.
        pub jwt_name: Mutex<Option<String>>,
        /// Refuse every token as DOCR does once the registry holds as many
        /// repositories as its plan allows.
        pub repository_limit: Mutex<bool>,
        pub base: Mutex<String>,
        /// digest → the manifest served for it; empty where not set.
        pub bodies: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl Fake {
        pub fn put(&self, repository: &str, tag: &str, digest: &str) {
            let mut manifests = self.manifests.lock().unwrap();
            let held = manifests.entry(repository.to_string()).or_default();
            held.insert(tag.to_string(), digest.to_string());
            held.insert(digest.to_string(), digest.to_string());
        }

        /// `manifest` under `tag`, by the digest of its bytes.
        pub fn put_manifest(&self, repository: &str, tag: &str, manifest: &[u8]) -> String {
            let digest = format!("sha256:{}", hex::encode(Sha256::digest(manifest)));
            self.put(repository, tag, &digest);
            self.bodies
                .lock()
                .unwrap()
                .insert(digest.clone(), manifest.to_vec());
            digest
        }
    }

    async fn token(State(fake): State<Arc<Fake>>, request: Request) -> Response {
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("pusher:secret")
        );
        let given = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        if given != Some(expected.as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let query = request.uri().query().unwrap_or_default().to_string();
        let scope = url_decode(
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("scope="))
                .unwrap_or_default(),
        );
        fake.scopes.lock().unwrap().push(scope.clone());
        if *fake.repository_limit.lock().unwrap() {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({ "errors": [{
                    "code": "DENIED",
                    "message": "registry contains 5 repositories, limit is 5"
                }]})),
            )
                .into_response();
        }
        if let Some(actions) = fake.jwt_actions.lock().unwrap().clone() {
            let named = fake.jwt_name.lock().unwrap().clone();
            let repository = named
                .clone()
                .unwrap_or_else(|| scope.split(':').nth(1).unwrap_or_default().to_string());
            let claims = match named.as_deref() {
                Some("") => serde_json::json!({ "sub": "pusher" }),
                _ => serde_json::json!({ "access": [
                    { "type": "repository", "name": repository, "actions": actions }
                ]}),
            };
            let payload =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
            return axum::Json(serde_json::json!({ "token": format!("granted.{payload}.sig") }))
                .into_response();
        }
        axum::Json(serde_json::json!({ "token": format!("granted {scope}") })).into_response()
    }

    fn url_decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            match bytes[at] {
                b'%' if at + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap();
                    out.push(u8::from_str_radix(hex, 16).unwrap());
                    at += 3;
                }
                b'+' => {
                    out.push(b' ');
                    at += 1;
                }
                byte => {
                    out.push(byte);
                    at += 1;
                }
            }
        }
        String::from_utf8(out).unwrap()
    }

    async fn registry(State(fake): State<Arc<Fake>>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();
        fake.seen.lock().unwrap().push(format!("{method} {path}"));
        if *fake.down.lock().unwrap() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let rest = path.strip_prefix("/v2/").unwrap_or_default();
        let (repository, reference) = match rest.rsplit_once("/manifests/") {
            Some((repository, reference)) => (repository.to_string(), reference.to_string()),
            None => (String::new(), String::new()),
        };
        let action = match method {
            Method::DELETE => "delete",
            _ => "pull",
        };
        let wanted = match repository.is_empty() {
            true => String::new(),
            false => format!("granted repository:{repository}:{action}"),
        };
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let basic = *fake.basic.lock().unwrap();
        let authorised = match (basic, repository.is_empty()) {
            (true, _) => {
                auth == format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode("pusher:secret")
                )
            }
            (false, true) => auth.starts_with("Bearer granted"),
            (false, false) => auth == format!("Bearer {wanted}"),
        };
        if !authorised && basic {
            let mut response = StatusCode::UNAUTHORIZED.into_response();
            response.headers_mut().insert(
                WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"fake\""),
            );
            return response;
        }
        if !authorised {
            let base = fake.base.lock().unwrap().clone();
            let mut challenge = format!("Bearer realm=\"{base}/token\",service=\"fake\"");
            if !repository.is_empty() {
                challenge.push_str(&format!(",scope=\"repository:{repository}:{action}\""));
            }
            let mut response = StatusCode::UNAUTHORIZED.into_response();
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_str(&challenge).unwrap());
            return response;
        }
        if repository.is_empty() {
            return StatusCode::OK.into_response();
        }
        let mut manifests = fake.manifests.lock().unwrap();
        let held = manifests.entry(repository).or_default();
        match method {
            Method::DELETE => {
                if *fake.refuse_delete.lock().unwrap() {
                    let body = serde_json::json!({"errors": [{"code": "UNSUPPORTED", "message": "deletion disabled"}]});
                    return (StatusCode::METHOD_NOT_ALLOWED, axum::Json(body)).into_response();
                }
                if !held.contains_key(&reference) {
                    return StatusCode::NOT_FOUND.into_response();
                }
                held.retain(|_, digest| *digest != reference);
                StatusCode::ACCEPTED.into_response()
            }
            _ => match held.get(&reference) {
                Some(digest) => {
                    let body = match method {
                        Method::GET => fake
                            .bodies
                            .lock()
                            .unwrap()
                            .get(digest)
                            .cloned()
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    let mut response = (StatusCode::OK, body).into_response();
                    if !*fake.no_digest_header.lock().unwrap() {
                        response.headers_mut().insert(
                            "docker-content-digest",
                            HeaderValue::from_str(digest).unwrap(),
                        );
                    }
                    response
                }
                None => StatusCode::NOT_FOUND.into_response(),
            },
        }
    }

    async fn digitalocean(State(fake): State<Arc<Fake>>, request: Request) -> Response {
        let path = request.uri().path().to_string();
        fake.seen
            .lock()
            .unwrap()
            .push(format!("{} {path}", request.method()));
        if request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            != Some("Bearer secret")
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let Some(rest) = path.strip_prefix("/api/v2/registries/") else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let mut parts = rest.split('/');
        let (registry, _, repository, _, digest) = (
            parts.next().unwrap_or_default(),
            parts.next(),
            parts.next().unwrap_or_default(),
            parts.next(),
            parts.next().unwrap_or_default(),
        );
        let repository = format!("{registry}/{}", url_decode(repository));
        let mut manifests = fake.manifests.lock().unwrap();
        let held = manifests.entry(repository).or_default();
        if !held.contains_key(digest) {
            return StatusCode::NOT_FOUND.into_response();
        }
        held.retain(|_, held| held != digest);
        StatusCode::NO_CONTENT.into_response()
    }

    /// Serve the fake; returns it with its `host:port`.
    pub(crate) async fn serve() -> (Arc<Fake>, String) {
        let fake = Arc::new(Fake::default());
        let app = axum::Router::new()
            .route("/token", axum::routing::get(token))
            .route("/api/{*rest}", axum::routing::any(digitalocean))
            .route("/v2/", axum::routing::any(registry))
            .route("/v2/{*rest}", axum::routing::any(registry))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        *fake.base.lock().unwrap() = format!("http://{host}");
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, host)
    }

    /// A Docker config holding `pusher:secret` for `host`.
    pub(crate) fn docker_config(host: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode("pusher:secret");
        std::fs::write(
            dir.path().join("config.json"),
            serde_json::json!({ "auths": { format!("https://{host}"): { "auth": auth } } })
                .to_string(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn references_are_taken_apart_as_a_registry_is_called() {
        let parsed =
            Reference::parse("registry.digitalocean.com/cosmicspork/tracon-repo-a:0123").unwrap();
        assert_eq!(parsed.host, "registry.digitalocean.com");
        assert_eq!(parsed.repository, "cosmicspork/tracon-repo-a");
        assert_eq!(parsed.reference, "0123");
        assert_eq!(
            parsed.name,
            "registry.digitalocean.com/cosmicspork/tracon-repo-a"
        );

        let parsed = Reference::parse(&format!("localhost:5000/a/b@{DIGEST}")).unwrap();
        assert_eq!(
            (parsed.host.as_str(), parsed.repository.as_str()),
            ("localhost:5000", "a/b")
        );
        assert!(parsed.is_digest());

        let hub = Reference::parse("rust").unwrap();
        assert_eq!(hub.host, "registry-1.docker.io");
        assert_eq!(hub.repository, "library/rust");
        assert_eq!(hub.reference, "latest");
        assert_eq!(
            Reference::parse("docker.io/oven/bun:1").unwrap().repository,
            "oven/bun"
        );

        for bad in [
            "",
            "host.io/UPPER:x",
            "host.io/a:b/c",
            "host.io/a@sha256:xyz",
            "host.io/a/../b",
            "host.io/a?x",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_challenge_is_read_with_commas_inside_its_scope() {
        assert_eq!(
            parse_challenge(
                r#"Bearer realm="https://auth.io/token",service="reg",scope="repository:a/b:pull,push""#
            ),
            Some(Challenge::Bearer {
                realm: "https://auth.io/token".into(),
                service: Some("reg".into()),
                scope: Some("repository:a/b:pull,push".into()),
            })
        );
        assert_eq!(
            parse_challenge(r#"Basic realm="Registry""#),
            Some(Challenge::Basic)
        );
        assert_eq!(parse_challenge("Negotiate"), None);
    }

    #[test]
    fn credentials_come_from_auth_or_from_user_and_password() {
        let dir = tempfile::tempdir().unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode("tok:tok-secret");
        std::fs::write(
            dir.path().join("config.json"),
            serde_json::json!({ "auths": {
                "registry.digitalocean.com": { "auth": auth },
                "https://index.docker.io/v1/": { "username": "me", "password": "pw" },
            }})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            credentials(dir.path(), "registry.digitalocean.com"),
            Ok(Some(("tok".into(), "tok-secret".into())))
        );
        assert_eq!(
            credentials(dir.path(), "registry-1.docker.io"),
            Ok(Some(("me".into(), "pw".into())))
        );
        assert_eq!(credentials(dir.path(), "ghcr.io"), Ok(None));
    }

    /// Two keys that name one host are refused rather than chosen between by
    /// map order, unless one of them is the host spelled exactly.
    #[test]
    fn two_credentials_for_one_host_are_refused_unless_one_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let encode = |pair: &str| base64::engine::general_purpose::STANDARD.encode(pair);
        let write = |auths: serde_json::Value| {
            std::fs::write(
                dir.path().join("config.json"),
                serde_json::json!({ "auths": auths }).to_string(),
            )
            .unwrap()
        };
        write(serde_json::json!({
            "https://reg.example.com": { "auth": encode("a:1") },
            "reg.example.com/v1/": { "auth": encode("b:2") },
        }));
        let error = credentials(dir.path(), "reg.example.com").unwrap_err();
        assert!(error.0.contains("more than one credential"), "{error}");
        write(serde_json::json!({
            "https://reg.example.com": { "auth": encode("a:1") },
            "reg.example.com": { "auth": encode("b:2") },
        }));
        assert_eq!(
            credentials(dir.path(), "reg.example.com"),
            Ok(Some(("b".into(), "2".into())))
        );
        write(serde_json::json!({ "reg.example.com": { "auth": "not base64!" } }));
        assert!(credentials(dir.path(), "reg.example.com").is_err());
    }

    /// A registry that asks for Basic is given the credential directly, for
    /// lookups, removal and the push check alike.
    #[tokio::test]
    async fn a_basic_challenge_is_answered_with_the_credential() {
        let (fake, host) = serve().await;
        *fake.basic.lock().unwrap() = true;
        fake.put("ns/app", "t1", DIGEST);
        let config = docker_config(&host);
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        assert_eq!(
            registry.identity(&format!("{host}/ns/app:t1")).await,
            Some(format!("{host}/ns/app@{DIGEST}"))
        );
        assert_eq!(
            registry.check_push(&format!("{host}/ns")).await,
            Ok(PushCheck::Unsaid)
        );
        registry.remove(&format!("{host}/ns/app:t1")).await.unwrap();
        assert!(!registry.exists(&format!("{host}/ns/app@{DIGEST}")).await);
        assert!(fake.scopes.lock().unwrap().is_empty());

        let anonymous = Registry::new(None).for_test("unused", "unused");
        let error = anonymous
            .digest(&format!("{host}/ns/app:t1"))
            .await
            .unwrap_err();
        assert!(error.0.contains("holds none"), "{error}");
    }

    /// A registry that does not answer leaves an image's presence unknown,
    /// never absent: what is standing is not rebuilt on its account.
    #[tokio::test]
    async fn a_registry_that_is_down_is_unknown_not_absent() {
        let (fake, host) = serve().await;
        fake.put("ns/app", "t1", DIGEST);
        let config = docker_config(&host);
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        let image = format!("{host}/ns/app@{DIGEST}");
        assert_eq!(
            registry.presence(&image).await,
            crate::boundary::Presence::Present
        );
        *fake.down.lock().unwrap() = true;
        assert_eq!(
            registry.presence(&image).await,
            crate::boundary::Presence::Unknown
        );
        *fake.down.lock().unwrap() = false;
        assert_eq!(
            registry.presence(&format!("{host}/ns/app:gone")).await,
            crate::boundary::Presence::Absent
        );
    }

    /// A registry at its repository limit refuses the probe repository, which
    /// does not exist; that leaves pushes to the repositories already there,
    /// so the check says so rather than failing every build. Any other
    /// refusal of the token still fails it, and says the registry's reason.
    #[tokio::test]
    async fn a_full_registry_is_no_refusal_of_the_credential() {
        let (fake, host) = serve().await;
        let config = docker_config(&host);
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        let prefix = format!("{host}/cs");
        *fake.repository_limit.lock().unwrap() = true;
        let Ok(PushCheck::NoNewRepositories(said)) = registry.check_push(&prefix).await else {
            panic!("a full registry is not a refusal");
        };
        assert!(
            said.contains("answered 403") && said.contains("DENIED: registry contains 5"),
            "{said}"
        );
    }

    #[test]
    fn a_refusal_says_the_registrys_own_words() {
        assert_eq!(
            refusal_message(r#"{"errors":[{"code":"DENIED","message":"no"}]}"#).as_deref(),
            Some("DENIED: no")
        );
        assert_eq!(refusal_message("forbidden\n").as_deref(), Some("forbidden"));
        assert_eq!(refusal_message("<html>…</html>"), None);
        assert_eq!(refusal_message(""), None);
        assert!(names_a_repository_limit(
            "DENIED: registry contains 5 repositories, limit is 5"
        ));
        assert!(!names_a_repository_limit(
            "DENIED: requested access to the resource is denied"
        ));
    }

    /// The push check asks for push and refuses a token that does not grant
    /// it; one that cannot be read is accepted without the claim.
    #[tokio::test]
    async fn a_read_only_credential_fails_the_push_check() {
        let (fake, host) = serve().await;
        let config = docker_config(&host);
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        let prefix = format!("{host}/cs");

        assert_eq!(registry.check_push(&prefix).await, Ok(PushCheck::Unsaid));
        *fake.jwt_actions.lock().unwrap() = Some(vec!["pull".into()]);
        let error = registry.check_push(&prefix).await.unwrap_err();
        assert!(
            error.0.contains("may not push") && error.0.contains("pull"),
            "{error}"
        );
        // A token that does not say, or says it of another name, is not a
        // refusal: push is not confirmed, and builds are not blocked on it.
        *fake.jwt_name.lock().unwrap() = Some("cs/something-else".into());
        assert_eq!(registry.check_push(&prefix).await, Ok(PushCheck::Unsaid));
        *fake.jwt_name.lock().unwrap() = Some(String::new());
        assert_eq!(registry.check_push(&prefix).await, Ok(PushCheck::Unsaid));
        *fake.jwt_name.lock().unwrap() = None;
        *fake.jwt_actions.lock().unwrap() = Some(vec!["pull".into(), "push".into()]);
        assert_eq!(registry.check_push(&prefix).await, Ok(PushCheck::Granted));
        // A token service that refuses the credential is a refusal.
        let wrong = tempfile::tempdir().unwrap();
        let auth = base64::engine::general_purpose::STANDARD.encode("pusher:wrong");
        std::fs::write(
            wrong.path().join("config.json"),
            serde_json::json!({ "auths": { host.clone(): { "auth": auth } } }).to_string(),
        )
        .unwrap();
        let refused = Registry::new(Some(wrong.path().to_path_buf()))
            .for_test("unused", "unused")
            .check_push(&prefix)
            .await
            .unwrap_err();
        assert!(refused.0.contains("answered 401"), "{refused}");
        assert!(fake
            .scopes
            .lock()
            .unwrap()
            .contains(&"repository:cs/tracon-repo-probe:push,pull".to_string()));
    }

    /// The client answers the token challenge with the configured credential,
    /// reads a tag's digest, says when nothing is there, and removes by
    /// digest, asking only for the scope each call needs.
    #[tokio::test]
    async fn the_client_answers_the_challenge_and_heads_and_deletes() {
        let (fake, host) = serve().await;
        fake.put("ns/app", "t1", DIGEST);
        let config = docker_config(&host);
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");

        assert_eq!(
            registry.identity(&format!("{host}/ns/app:t1")).await,
            Some(format!("{host}/ns/app@{DIGEST}"))
        );
        assert!(registry.exists(&format!("{host}/ns/app@{DIGEST}")).await);
        assert!(!registry.exists(&format!("{host}/ns/app:missing")).await);
        assert_eq!(
            registry.digest(&format!("{host}/ns/other:t1")).await,
            Ok(None)
        );
        assert!(registry.check_push(&format!("{host}/ns")).await.is_ok());

        registry.remove(&format!("{host}/ns/app:t1")).await.unwrap();
        assert!(!registry.exists(&format!("{host}/ns/app@{DIGEST}")).await);
        // Gone already is removed.
        registry
            .remove(&format!("{host}/ns/app@{DIGEST}"))
            .await
            .unwrap();

        let scopes = fake.scopes.lock().unwrap().clone();
        assert!(
            scopes.contains(&"repository:ns/app:pull".to_string()),
            "{scopes:?}"
        );
        assert!(
            scopes.contains(&"repository:ns/app:delete".to_string()),
            "{scopes:?}"
        );
    }

    /// Without a credential the token service refuses, and the error says
    /// the Docker config is why.
    #[tokio::test]
    async fn a_missing_credential_is_named() {
        let (fake, host) = serve().await;
        fake.put("ns/app", "t1", DIGEST);
        let registry = Registry::new(None).for_test("unused", "unused");
        let error = registry
            .digest(&format!("{host}/ns/app:t1"))
            .await
            .unwrap_err();
        assert!(error.0.contains("holds no credential"), "{error}");
        assert!(registry
            .check_push(&format!("{host}/ns"))
            .await
            .unwrap_err()
            .0
            .contains("no credential"));
    }

    pub(crate) const AMD64: &str =
        "sha256:c9d629a83e51fc14e9d563e2db88ad1356f03d27c86811e54295807c6e974e97";
    pub(crate) const ARM64: &str =
        "sha256:83f7a7484c46f20af5306f10b104949c4c8a2368681a3ab8608e8674da553c3d";

    /// An OCI index as the release pushes the base: each platform's manifest
    /// and an attestation for each, whose digests change with every build
    /// even when the image does not.
    pub(crate) fn oci_index(attestation: char) -> String {
        let attestation = |of: &str| {
            serde_json::json!({
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{}", attestation.to_string().repeat(64)),
                "size": 838,
                "platform": {"architecture": "unknown", "os": "unknown"},
                "annotations": {
                    "vnd.docker.reference.digest": of,
                    "vnd.docker.reference.type": "attestation-manifest"
                }
            })
        };
        serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": ARM64,
                 "size": 1819, "platform": {"architecture": "arm64", "os": "linux"}},
                attestation(ARM64),
                {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": AMD64,
                 "size": 1819, "platform": {"architecture": "amd64", "os": "linux"}},
                attestation(AMD64),
            ]
        })
        .to_string()
    }

    #[test]
    fn an_index_names_each_platforms_manifest() {
        let index = oci_index('a');
        assert_eq!(
            platform_entry(index.as_bytes(), "amd64")
                .unwrap()
                .as_deref(),
            Some(AMD64)
        );
        assert_eq!(
            platform_entry(index.as_bytes(), "arm64")
                .unwrap()
                .as_deref(),
            Some(ARM64)
        );
        let missing = platform_entry(index.as_bytes(), "s390x").unwrap_err();
        assert!(missing.0.contains("no linux/s390x image"), "{missing}");
    }

    #[test]
    fn a_docker_manifest_list_names_arm64_by_its_v8_variant() {
        let v7 = format!("sha256:{}", "7".repeat(64));
        let list = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": [
                {"mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                 "digest": v7, "size": 1,
                 "platform": {"architecture": "arm", "os": "linux", "variant": "v7"}},
                {"mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                 "digest": format!("sha256:{}", "6".repeat(64)), "size": 1,
                 "platform": {"architecture": "arm64", "os": "linux", "variant": "v9"}},
                {"mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                 "digest": ARM64, "size": 1,
                 "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}},
                {"mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                 "digest": format!("sha256:{}", "5".repeat(64)), "size": 1,
                 "platform": {"architecture": "amd64", "os": "windows"}},
                {"mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                 "digest": AMD64, "size": 1,
                 "platform": {"architecture": "amd64", "os": "linux"}},
            ]
        })
        .to_string();
        assert_eq!(
            platform_entry(list.as_bytes(), "arm64").unwrap().as_deref(),
            Some(ARM64)
        );
        assert_eq!(
            platform_entry(list.as_bytes(), "amd64").unwrap().as_deref(),
            Some(AMD64)
        );
    }

    #[test]
    fn a_single_manifest_is_its_own_platforms() {
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": format!("sha256:{}", "c".repeat(64)), "size": 1},
            "layers": []
        })
        .to_string();
        assert_eq!(platform_entry(manifest.as_bytes(), "amd64").unwrap(), None);
        assert!(platform_entry(b"<html>", "amd64").is_err());
    }

    /// A tag on an index is resolved to the platform's entry; a digest of a
    /// single manifest is itself; a name the registry does not hold is none.
    #[tokio::test]
    async fn a_platform_identity_is_the_platforms_manifest() {
        let (fake, host) = serve().await;
        fake.put_manifest("ns/base", "1", oci_index('a').as_bytes());
        let single = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[]}"#;
        let single_digest = fake.put_manifest("ns/base", "single", single);
        let config = docker_config(&host);
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        assert_eq!(
            registry
                .platform_identity(&format!("{host}/ns/base:1"), "amd64")
                .await
                .unwrap(),
            Some(format!("{host}/ns/base@{AMD64}"))
        );
        let pinned = format!("{host}/ns/base@{single_digest}");
        assert_eq!(
            registry.platform_identity(&pinned, "arm64").await.unwrap(),
            Some(pinned.clone())
        );
        assert_eq!(
            registry
                .platform_identity(&format!("{host}/ns/base:gone"), "amd64")
                .await
                .unwrap(),
            None
        );
    }

    /// A registry that leaves the digest header off is read by hashing what
    /// it serves.
    #[tokio::test]
    async fn a_digest_without_its_header_is_the_manifests_hash() {
        let (fake, host) = serve().await;
        fake.put("ns/app", "t1", DIGEST);
        *fake.no_digest_header.lock().unwrap() = true;
        let config = docker_config(&host);
        let registry =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        let digest = registry
            .digest(&format!("{host}/ns/app:t1"))
            .await
            .unwrap()
            .unwrap();
        // The fake serves an empty body.
        assert_eq!(
            digest,
            format!("sha256:{}", hex::encode(Sha256::digest(b"")))
        );
    }

    /// DigitalOcean's registry refusing a manifest delete falls back to its
    /// API, with the nested repository encoded; any other registry's refusal
    /// is reported.
    #[tokio::test]
    async fn a_refused_delete_falls_back_to_the_digitalocean_api() {
        let (fake, host) = serve().await;
        fake.put("cosmicspork/tracon/app", "t1", DIGEST);
        *fake.refuse_delete.lock().unwrap() = true;
        let config = docker_config(&host);

        let elsewhere =
            Registry::new(Some(config.path().to_path_buf())).for_test("unused", "unused");
        let error = elsewhere
            .remove(&format!("{host}/cosmicspork/tracon/app@{DIGEST}"))
            .await
            .unwrap_err();
        assert!(error.0.contains("UNSUPPORTED"), "{error}");

        let api = format!("http://{host}/api");
        let digitalocean = Registry::new(Some(config.path().to_path_buf())).for_test(&host, &api);
        digitalocean
            .remove(&format!("{host}/cosmicspork/tracon/app:t1"))
            .await
            .unwrap();
        let seen = fake.seen.lock().unwrap().clone();
        assert!(
            seen.contains(&format!(
                "DELETE /api/v2/registries/cosmicspork/repositories/tracon%2Fapp/digests/{DIGEST}"
            )),
            "{seen:?}"
        );
        assert!(
            !digitalocean
                .exists(&format!("{host}/cosmicspork/tracon/app@{DIGEST}"))
                .await
        );

        // A configured API token is presented instead of the credential.
        fake.put("cosmicspork/app", "t2", DIGEST);
        let token = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(token.path(), "wrong\n").unwrap();
        let with_token = Registry::new(Some(config.path().to_path_buf()))
            .with_digitalocean_token(Some(token.path().to_path_buf()))
            .for_test(&host, &api);
        let error = with_token
            .remove(&format!("{host}/cosmicspork/app:t2"))
            .await
            .unwrap_err();
        assert!(
            error
                .0
                .contains("through the DigitalOcean API: answered 401"),
            "{error}"
        );
    }
}
