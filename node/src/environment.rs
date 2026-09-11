//! Safe project preparation in a runtime-owned workspace.  Preparation is a
//! separate, credential-free runner command; it never interprets repository
//! setup hooks or grants the agent an additional mount.

use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    boundary::Backend,
    config::Config,
    runner::{Mount, RunnerCommand},
    workspace::Workspace,
};

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid devcontainer.json: {0}")]
    Devcontainer(String),
    #[error("devcontainer requires a pinned image or an operator-approved image")]
    UnapprovedImage,
    #[error("devcontainer field {0} is not permitted for restricted execution")]
    UnsafeDevcontainer(&'static str),
    #[error("no supported locked dependency input was found")]
    NoDependencyInput,
    #[error("runtime refused: {0}")]
    Runtime(String),
    #[error("preparation command failed ({exit}): {tail}")]
    PrepareFailed { exit: i32, tail: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyInput {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparationPlan {
    /// The project-selected image. None means the already-approved harness
    /// image, which is the safe fallback for conventional lockfile projects.
    pub image: Option<String>,
    pub dependency_inputs: Vec<DependencyInput>,
    pub command: String,
    pub cache_volume: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreparedEnvironment {
    pub image: String,
    pub cache_volume: String,
    pub dependency_inputs: Vec<DependencyInput>,
    pub command: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    pub command: String,
    pub ok: bool,
    pub exit: i32,
    pub tail: String,
    pub elapsed_ms: u64,
}

/// Inspect normal project conventions without executing repository-controlled
/// commands. `devcontainer.json` supplies an image only when it is a plain,
/// pinned image configuration; setup hooks, mounts, sockets, privilege, and
/// feature installers are rejected rather than partially honored.
pub fn inspect(workspace: &Path) -> Result<PreparationPlan, EnvironmentError> {
    let image = inspect_devcontainer(workspace)?;
    let mut inputs = Vec::new();
    let mut command = None;
    for (file, prepare) in [
        ("Cargo.lock", "cargo fetch --locked"),
        ("package-lock.json", "npm ci --ignore-scripts"),
        ("npm-shrinkwrap.json", "npm ci --ignore-scripts"),
        ("bun.lock", "bun install --frozen-lockfile --ignore-scripts"),
        ("bun.lockb", "bun install --frozen-lockfile --ignore-scripts"),
        ("pnpm-lock.yaml", "pnpm install --frozen-lockfile --ignore-scripts"),
        ("yarn.lock", "yarn install --frozen-lockfile --ignore-scripts"),
        ("composer.lock", "composer install --no-interaction --no-scripts --prefer-dist"),
    ] {
        let path = workspace.join(file);
        if path.is_file() {
            inputs.push(DependencyInput {
                path: file.into(),
                sha256: file_hash(&path)?,
            });
            command.get_or_insert(prepare);
        }
    }
    let command = command.ok_or(EnvironmentError::NoDependencyInput)?;
    let mut hash = Sha256::new();
    if let Some(image) = &image {
        hash.update(image.as_bytes());
    }
    for input in &inputs {
        hash.update(input.path.as_bytes());
        hash.update(input.sha256.as_bytes());
    }
    Ok(PreparationPlan {
        image,
        dependency_inputs: inputs,
        command: command.to_string(),
        cache_volume: format!("tracon-cache-{}", hex::encode(hash.finalize())[..24].to_string()),
    })
}

/// Execute dependency preparation in an isolated cache. Only a project image
/// with an immutable digest, or one an operator explicitly approved in config,
/// can replace the node's own prepared runner image.
pub async fn prepare(
    backend: &dyn Backend,
    cfg: &Config,
    workspace: &Workspace,
    plan: &PreparationPlan,
) -> Result<PreparedEnvironment, EnvironmentError> {
    let image = approved_image(cfg, plan.image.as_deref())?;
    let runner = backend.runner(Vec::new());
    let started = Instant::now();
    let output = runner
        .run_capture(RunnerCommand {
            argv: vec!["sh".into(), "-lc".into(), plan.command.clone()],
            env: vec![
                ("HOME".into(), "/cache/home".into()),
                ("CARGO_HOME".into(), "/cache/cargo".into()),
                ("npm_config_cache".into(), "/cache/npm".into()),
                ("BUN_INSTALL_CACHE_DIR".into(), "/cache/bun".into()),
                ("PIP_CACHE_DIR".into(), "/cache/pip".into()),
                ("COMPOSER_CACHE_DIR".into(), "/cache/composer".into()),
            ],
            mounts: vec![
                workspace.mount("/work", false),
                Mount::volume(plan.cache_volume.clone(), "/cache", false),
            ],
            workdir: Some("/work".into()),
            image: Some(image.clone()),
            name: format!("tracon-prepare-{}", workspace.id),
        })
        .await
        .map_err(|e| EnvironmentError::Runtime(e.to_string()))?;
    let exit = output.status.code().unwrap_or(-1);
    let tail = tail(&[output.stdout, output.stderr].concat());
    if !output.status.success() {
        return Err(EnvironmentError::PrepareFailed { exit, tail });
    }
    Ok(PreparedEnvironment {
        image,
        cache_volume: plan.cache_volume.clone(),
        dependency_inputs: plan.dependency_inputs.clone(),
        command: plan.command.clone(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Run operator-selected candidate verification in a fresh restricted command.
/// The caller supplies commands from node policy, never from repository files
/// or an agent tool request; the candidate workspace gets no broker credential.
pub async fn verify(
    backend: &dyn Backend,
    workspace: &Workspace,
    prepared: &PreparedEnvironment,
    commands: &[String],
) -> Result<Vec<Verification>, EnvironmentError> {
    let runner = backend.runner(Vec::new());
    let mut result = Vec::new();
    for (n, command) in commands.iter().enumerate() {
        if command.trim().is_empty() {
            continue;
        }
        let started = Instant::now();
        let output = runner
            .run_capture(RunnerCommand {
                argv: vec!["sh".into(), "-lc".into(), command.clone()],
                env: Vec::new(),
                mounts: vec![
                    workspace.mount("/work", true),
                    Mount::volume(prepared.cache_volume.clone(), "/cache", true),
                ],
                workdir: Some("/work".into()),
                image: Some(prepared.image.clone()),
                name: format!("tracon-verify-{}-{n}", workspace.id),
            })
            .await
            .map_err(|e| EnvironmentError::Runtime(e.to_string()))?;
        result.push(Verification {
            command: command.clone(),
            ok: output.status.success(),
            exit: output.status.code().unwrap_or(-1),
            tail: tail(&[output.stdout, output.stderr].concat()),
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    }
    Ok(result)
}

fn inspect_devcontainer(workspace: &Path) -> Result<Option<String>, EnvironmentError> {
    let path = workspace.join(".devcontainer/devcontainer.json");
    if !path.exists() {
        return Ok(None);
    }
    let source = std::fs::read_to_string(&path)?;
    let value: Value = serde_json::from_str(&source)
        .map_err(|e| EnvironmentError::Devcontainer(e.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| EnvironmentError::Devcontainer("top level must be an object".into()))?;
    for unsafe_key in [
        "privileged",
        "capAdd",
        "mounts",
        "workspaceMount",
        "runArgs",
        "initializeCommand",
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "features",
        "containerEnv",
        "remoteEnv",
    ] {
        if object.get(unsafe_key).is_some_and(non_empty) {
            return Err(EnvironmentError::UnsafeDevcontainer(unsafe_key));
        }
    }
    match object.get("image") {
        Some(Value::String(image)) if !image.trim().is_empty() => Ok(Some(image.clone())),
        Some(_) => Err(EnvironmentError::Devcontainer("image must be a string".into())),
        None => Err(EnvironmentError::Devcontainer(
            "a build-based devcontainer is not allowed; use an approved image digest".into(),
        )),
    }
}

fn non_empty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(false) => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
        _ => true,
    }
}

fn approved_image(cfg: &Config, selected: Option<&str>) -> Result<String, EnvironmentError> {
    match selected {
        None => Ok(match cfg.runtime.kind {
            crate::config::RuntimeKind::Podman => cfg.boundary.harness_image.clone(),
            crate::config::RuntimeKind::Kubernetes => cfg.runtime.kubernetes.harness_image.clone(),
        }),
        Some(image) if image.contains("@sha256:") || cfg.runtime.approved_images.iter().any(|allowed| allowed == image) => {
            Ok(image.to_string())
        }
        Some(_) => Err(EnvironmentError::UnapprovedImage),
    }
}

fn file_hash(path: &Path) -> Result<String, std::io::Error> {
    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut input, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn tail(bytes: &[u8]) -> String {
    const MAX: usize = 4096;
    let start = bytes.len().saturating_sub(MAX);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}
