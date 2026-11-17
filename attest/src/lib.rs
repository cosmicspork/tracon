//! Release attestation verification, shared by the desktop wrapper and the
//! node. GitHub's build provenance is a Sigstore bundle; checking it is left
//! to a digest-pinned `gh`, which the caller supplies, so this crate holds no
//! crypto of its own. What it does hold is the part around `gh`: fetching a
//! bundle under a size cap, the exact arguments that say what a signature must
//! prove, and the attestation lookup by digest a plugin install needs.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
};

/// Where `verify_release_artifact` looks attestations up, outside tests.
pub const GITHUB_API: &str = "https://api.github.com";
/// A bundle is signed data but is still bounded before `gh` parses it. The
/// same cap bounds an attestation lookup's response, which carries bundles.
pub const BUNDLE_MAX_BYTES: u64 = 8 * 1024 * 1024;
const API_ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
/// The verifier's stderr is carried as detail, so a runaway one is cut short.
const DETAIL_MAX_BYTES: usize = 2048;

/// What a signature must prove, beyond being Sigstore-valid for the artifact.
#[derive(Debug, Clone, Copy)]
pub struct Policy<'a> {
    /// `owner/name` the attestation must come from.
    pub repo: &'a str,
    /// `owner/name/.github/workflows/<file>` that must have signed it.
    pub signer_workflow: Option<&'a str>,
    /// The git ref the signing run must have been built from.
    pub source_ref: Option<&'a str>,
    pub deny_self_hosted_runners: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not fetch the attestation bundle: {0}")]
    Bundle(String),
    #[error("could not run the verifier: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the verifier did not finish in time")]
    Timeout,
    #[error("the attestation did not verify: {detail}")]
    Rejected { detail: String },
}

/// The result of looking an artifact's attestation up and checking it.
/// `Absent` is said only when GitHub answered that there is none: a lookup
/// that could not be made is `Failed`, never "no attestation".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Verified,
    Absent,
    Failed(String),
}

/// A `gh` binary, run with no credentials of its own.
#[derive(Debug, Clone)]
pub struct Verifier {
    path: PathBuf,
    timeout: Duration,
    trusted_root: Option<PathBuf>,
}

impl Verifier {
    pub fn new(path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            path: path.into(),
            timeout,
            trusted_root: None,
        }
    }

    /// Verify against this Sigstore trusted root (`gh attestation
    /// trusted-root`'s output) instead of fetching the current one.
    pub fn with_trusted_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.trusted_root = Some(path.into());
        self
    }

    /// Check a local JSONL bundle against `artifact` and `policy`. Success is
    /// `gh` exiting 0; anything else is `Rejected` with what it wrote to
    /// stderr.
    pub async fn verify_bundle(
        &self,
        artifact: &Path,
        bundle: &Path,
        policy: &Policy<'_>,
    ) -> Result<(), Error> {
        let output = tokio::time::timeout(
            self.timeout,
            tokio::process::Command::new(&self.path)
                .args(self.arguments(artifact, bundle, policy))
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(Error::Spawn)?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = match stderr.trim() {
            "" => output.status.to_string(),
            stderr => truncate(stderr, DETAIL_MAX_BYTES).to_string(),
        };
        Err(Error::Rejected { detail })
    }

    fn arguments(&self, artifact: &Path, bundle: &Path, policy: &Policy<'_>) -> Vec<OsString> {
        let mut arguments: Vec<OsString> = vec![
            "attestation".into(),
            "verify".into(),
            artifact.into(),
            "--bundle".into(),
            bundle.into(),
            "--hostname".into(),
            "github.com".into(),
            "--repo".into(),
            policy.repo.into(),
        ];
        if let Some(workflow) = policy.signer_workflow {
            arguments.extend(["--signer-workflow".into(), workflow.into()]);
        }
        if let Some(source_ref) = policy.source_ref {
            arguments.extend(["--source-ref".into(), source_ref.into()]);
        }
        if policy.deny_self_hosted_runners {
            arguments.push("--deny-self-hosted-runners".into());
        }
        if let Some(root) = &self.trusted_root {
            arguments.extend(["--custom-trusted-root".into(), root.into()]);
        }
        arguments
    }

    /// Look the attestation for `digest` up in `policy.repo` and verify it
    /// against `artifact`, which must be the file with that digest: the file
    /// is hashed first, so `Verified` is always about the bytes on disk.
    pub async fn verify_release_artifact(
        &self,
        client: &reqwest::Client,
        api_base: &str,
        artifact: &Path,
        digest: &[u8; 32],
        policy: &Policy<'_>,
    ) -> Outcome {
        match sha256_file(artifact).await {
            Ok(actual) if actual == *digest => {}
            Ok(_) => return Outcome::Failed("the artifact does not have the digest given".into()),
            Err(error) => return Outcome::Failed(format!("could not read the artifact: {error}")),
        }
        let bundles = match self.lookup(client, api_base, digest, policy).await {
            Ok(Some(bundles)) => bundles,
            Ok(None) => return Outcome::Absent,
            Err(detail) => return Outcome::Failed(detail),
        };

        let Some(name) = artifact.file_name() else {
            return Outcome::Failed("the artifact path has no file name".into());
        };
        let bundle =
            artifact.with_file_name(format!(".{}.attestation.jsonl", name.to_string_lossy()));
        let verified = async {
            write_new(&bundle, bundles.as_bytes())
                .await
                .map_err(|error| format!("could not store the bundle: {error}"))?;
            self.verify_bundle(artifact, &bundle, policy)
                .await
                .map_err(|error| error.to_string())
        }
        .await;
        let _ = fs::remove_file(&bundle).await;
        match verified {
            Ok(()) => Outcome::Verified,
            Err(detail) => Outcome::Failed(detail),
        }
    }

    /// The repository's attestations for `digest`, as JSONL bundles, or
    /// `None` when GitHub says there are none.
    async fn lookup(
        &self,
        client: &reqwest::Client,
        api_base: &str,
        digest: &[u8; 32],
        policy: &Policy<'_>,
    ) -> Result<Option<String>, String> {
        let url = format!(
            "{}/repos/{}/attestations/sha256:{}",
            api_base.trim_end_matches('/'),
            policy.repo,
            hex::encode(digest)
        );
        let response = client
            .get(&url)
            .header(reqwest::header::ACCEPT, API_ACCEPT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(
                reqwest::header::USER_AGENT,
                concat!("tracon/", env!("CARGO_PKG_VERSION")),
            )
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|error| format!("attestation lookup failed: {error}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(format!("attestation lookup answered {}", response.status()));
        }
        let body = read_capped(response, BUNDLE_MAX_BYTES).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| format!("attestation lookup answered bad JSON: {error}"))?;
        let attestations = body
            .get("attestations")
            .and_then(|list| list.as_array())
            .ok_or("attestation lookup answered without an attestations list")?;
        if attestations.is_empty() {
            return Ok(None);
        }
        let mut lines = String::new();
        for attestation in attestations {
            let bundle = attestation
                .get("bundle")
                .filter(|bundle| bundle.is_object())
                .ok_or("an attestation came without an inline bundle")?;
            lines.push_str(&bundle.to_string());
            lines.push('\n');
        }
        Ok(Some(lines))
    }
}

/// Store a bundle at `path`. It is not trusted until `gh` verifies it, so it
/// is bounded by `max_bytes`, both as announced and as streamed, and never
/// written through an existing file: whatever is there is removed first and
/// the bundle goes to a new one. On failure nothing is left behind.
pub async fn download_bundle(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    max_bytes: u64,
) -> Result<(), Error> {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Bundle(error.to_string())),
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|error| Error::Bundle(error.to_string()))?;

    let transfer = async {
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes)
        {
            return Err("the bundle is larger than allowed".to_string());
        }
        let mut stream = response.bytes_stream();
        let mut written = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| error.to_string())?;
            written = written
                .checked_add(chunk.len() as u64)
                .filter(|length| *length <= max_bytes)
                .ok_or("the bundle is larger than allowed")?;
            output
                .write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
        }
        output.sync_all().await.map_err(|error| error.to_string())
    }
    .await;

    drop(output);
    if transfer.is_err() {
        let _ = fs::remove_file(path).await;
    }
    transfer.map_err(Error::Bundle)
}

async fn read_capped(response: reqwest::Response, max_bytes: u64) -> Result<Vec<u8>, String> {
    let too_large = || "attestation lookup answered more than allowed".to_string();
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("attestation lookup failed: {error}"))?;
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..read]);
    }
}

async fn write_new(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(contents).await?;
    file.sync_all().await
}

fn truncate(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
