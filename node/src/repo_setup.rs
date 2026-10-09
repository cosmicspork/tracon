//! Setting a repository up once.
//!
//! A `[[repo]]` entry — the image, the preparation, the checks and what
//! preparation may reach — is the operator's, and writing one by hand means
//! reading the repository for what it already says. These are the node's
//! tools for that reading: a draft from what the default branch holds (its
//! devcontainer, its lockfiles, its `just` recipe or `package.json`
//! scripts), a trial of a draft in a fresh container, and a proposal the
//! operator answers on a card. The node writes the entry when the operator
//! approves it, never the agent: a proposal is always an approval, whatever
//! the policy says, and an agent can neither waive a check nor open its own
//! sessions' network by proposing.
//!
//! Everything is read from the default branch's commit, not a session's
//! workspace, so what a session wrote cannot shape the draft or the trial.
//! A trial prepares with egress only to the hosts the asking session can
//! already reach: a host the draft names that it cannot is reported as not
//! tried, and asking for it (`request_egress`) is how it becomes reachable.
//! Hosts the operator opened to the session that way are suggested in the
//! draft's `egress`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::boundary::Backend;
use crate::config::{Config, Repo};
use crate::runner::{Mount, RunnerCommand};
use crate::store::Store;

/// Where a drafted field came from, for the agent to check and the operator
/// to read.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Source {
    /// The file it was read from, relative to the repository root, or the
    /// approval it was opened by.
    pub from: String,
    /// What it gave the draft.
    pub gave: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Draft {
    pub repo: PathBuf,
    /// The default branch's commit the draft was read at.
    pub commit: String,
    pub entry: Repo,
    /// The entry the table holds for the repository now, if any.
    pub current: Option<Repo>,
    pub sources: Vec<Source>,
    /// What the draft could not settle, for the agent to decide.
    pub notes: Vec<String>,
}

/// Each lockfile: what preparation runs for it and the preset its downloads
/// come from. One JavaScript lockfile per directory, in this order.
const LOCKFILES: &[(&str, &str, &str)] = &[
    ("Cargo.lock", "cargo fetch --locked", "crates"),
    (
        "bun.lock",
        "bun install --frozen-lockfile --ignore-scripts",
        "npm",
    ),
    (
        "bun.lockb",
        "bun install --frozen-lockfile --ignore-scripts",
        "npm",
    ),
    (
        "pnpm-lock.yaml",
        "pnpm install --frozen-lockfile --ignore-scripts",
        "npm",
    ),
    (
        "yarn.lock",
        "yarn install --frozen-lockfile --ignore-scripts",
        "npm",
    ),
    ("package-lock.json", "npm ci --ignore-scripts", "npm"),
    (
        "composer.lock",
        "composer install --no-scripts --no-interaction",
        "packagist",
    ),
    ("uv.lock", "uv sync --frozen", "pypi"),
];

const JS_LOCKFILES: &[&str] = &[
    "bun.lock",
    "bun.lockb",
    "pnpm-lock.yaml",
    "yarn.lock",
    "package-lock.json",
];

/// The `package.json` scripts a check is drafted from, in order.
const CHECK_SCRIPTS: &[&str] = &["check", "lint", "typecheck", "test"];

/// Directories never looked into for a lockfile.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target", "vendor", "dist", "build"];

/// Draft an entry for the repository whose default-branch tree is at `root`.
/// `approved` are hosts the operator opened to the asking session, each with
/// the approval that opened it.
pub fn draft_from_tree(
    repo: &Path,
    root: &Path,
    current: Option<&Repo>,
    approved: &[(String, String)],
) -> (Repo, Vec<Source>, Vec<String>) {
    let mut entry = Repo {
        path: current
            .map(|entry| entry.path.clone())
            .unwrap_or_else(|| repo.to_path_buf()),
        // Not the agent's to propose: opening a repository's egress to its
        // sessions is the operator's call, made in Settings or on an egress
        // card, and a proposal keeps whatever the entry says now.
        session_egress: current.is_some_and(|entry| entry.session_egress),
        timeout_secs: current.and_then(|entry| entry.timeout_secs),
        ..Default::default()
    };
    let mut sources = Vec::new();
    let mut notes = Vec::new();

    draft_image(root, &mut entry, &mut sources, &mut notes);

    let mut presets = BTreeSet::new();
    for dir in lockfile_dirs(root) {
        let at = root.join(&dir);
        let mut js_done = false;
        for (file, command, preset) in LOCKFILES {
            if !at.join(file).is_file() {
                continue;
            }
            if JS_LOCKFILES.contains(file) {
                if js_done {
                    continue;
                }
                js_done = true;
            }
            let command = match dir.as_str() {
                "" => command.to_string(),
                dir => format!("cd {dir} && {command}"),
            };
            sources.push(Source {
                from: join(&dir, file),
                gave: format!("prepare `{command}`, egress `{preset}`"),
            });
            entry.prepare.push(command);
            presets.insert(preset.to_string());
        }
    }
    entry.egress.extend(presets);
    let covered: BTreeSet<String> = entry
        .egress_hosts()
        .unwrap_or_default()
        .into_iter()
        .collect();
    for (host, approval) in approved {
        if !covered.contains(host) && !entry.egress.contains(host) {
            sources.push(Source {
                from: format!("approval {approval}"),
                gave: format!("egress `{host}`, opened to this session by the operator"),
            });
            entry.egress.push(host.clone());
        }
    }

    let checks = draft_checks(root, &mut sources);
    if checks.is_empty() {
        notes.push(
            "no check was found: name the commands that must pass before a change is \
             reviewed (a `just` recipe, a `package.json` script, `cargo test`)"
                .into(),
        );
    }
    entry.checks = Some(checks);
    if entry.prepare.is_empty() {
        notes.push("no lockfile was found, so nothing is prepared before the checks".into());
    }
    if let Err(error) = crate::config::validate_repos(std::slice::from_ref(&entry)) {
        notes.push(format!(
            "the node would refuse this entry as drafted: {error}"
        ));
    }
    (entry, sources, notes)
}

/// The image: a devcontainer's pinned `image` or its Dockerfile, or a
/// Dockerfile in `.devcontainer/` with no `devcontainer.json` to point at it.
fn draft_image(root: &Path, entry: &mut Repo, sources: &mut Vec<Source>, notes: &mut Vec<String>) {
    let candidates = [".devcontainer/devcontainer.json", ".devcontainer.json"];
    let found = candidates
        .iter()
        .find(|relative| root.join(relative).is_file());
    if let Some(relative) = found {
        let text = std::fs::read_to_string(root.join(relative)).unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(&strip_jsonc(&text)).ok();
        let Some(config) = parsed else {
            notes.push(format!(
                "{relative} is not JSON the node can read, so no image was drafted from it"
            ));
            return;
        };
        let base = Path::new(relative).parent().unwrap_or(Path::new(""));
        let dockerfile = config["build"]["dockerfile"]
            .as_str()
            .or_else(|| config["build"]["dockerFile"].as_str())
            .or_else(|| config["dockerFile"].as_str());
        if let Some(dockerfile) = dockerfile {
            let path = normalize(&base.join(dockerfile));
            let context = config["build"]["context"]
                .as_str()
                .or_else(|| config["context"].as_str())
                .and_then(|context| normalize(&base.join(context)))
                // The repository root is `.` to the table.
                .map(|context| {
                    if context.is_empty() {
                        ".".into()
                    } else {
                        context
                    }
                });
            if path
                .as_deref()
                .is_some_and(|path| root.join(path).is_file())
            {
                entry.dockerfile = path.clone();
                // The default context is the Dockerfile's own directory.
                let default = path
                    .as_deref()
                    .and_then(|path| Path::new(path).parent())
                    .map(|dir| dir.to_string_lossy().to_string());
                if context.is_some() && context != default {
                    entry.context = context;
                }
                sources.push(Source {
                    from: relative.to_string(),
                    gave: format!(
                        "dockerfile `{}`; the node builds it from the default branch",
                        path.unwrap_or_default()
                    ),
                });
            } else {
                notes.push(format!(
                    "{relative} names a Dockerfile ({dockerfile}) the default branch does not hold"
                ));
            }
        } else if let Some(image) = config["image"].as_str() {
            if image.contains("@sha256:") {
                entry.image = Some(image.to_string());
                sources.push(Source {
                    from: relative.to_string(),
                    gave: format!("image `{image}`"),
                });
            } else {
                notes.push(format!(
                    "{relative} names `{image}` by tag; an entry's image must be pinned by \
                     digest (`name@sha256:…`), so pull it and pin what you pulled"
                ));
            }
        }
        if config.get("dockerComposeFile").is_some() {
            notes.push(format!(
                "{relative} is a compose setup; only one image is honoured, and a service it \
                 starts belongs in a node's service catalogue instead"
            ));
        }
        for hook in [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
        ] {
            if config.get(hook).is_some() {
                notes.push(format!(
                    "{relative} has a `{hook}`, which the node never runs: what it installs \
                     belongs in `prepare` or the Dockerfile"
                ));
            }
        }
        return;
    }
    if root.join(".devcontainer/Dockerfile").is_file() {
        entry.dockerfile = Some(".devcontainer/Dockerfile".into());
        sources.push(Source {
            from: ".devcontainer/Dockerfile".into(),
            gave: "dockerfile `.devcontainer/Dockerfile`; the node builds it from the default \
                   branch"
                .into(),
        });
        return;
    }
    notes.push(
        "no devcontainer was found: the checks run in the node's harness image unless the \
         entry names a pinned `image` or a `dockerfile`"
            .into(),
    );
}

/// The checks: a `just check` recipe over everything else, then the root
/// `package.json`'s check scripts, then Cargo's tests.
fn draft_checks(root: &Path, sources: &mut Vec<Source>) -> Vec<String> {
    for name in ["justfile", "Justfile", ".justfile"] {
        let Ok(text) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        if text.lines().any(is_check_recipe) {
            sources.push(Source {
                from: name.into(),
                gave: "check `just check`".into(),
            });
            return vec!["just check".into()];
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("package.json")) {
        let scripts = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|package| package.get("scripts").cloned())
            .unwrap_or(Value::Null);
        let runner = JS_LOCKFILES
            .iter()
            .find(|file| root.join(file).is_file())
            .map(|file| match *file {
                "bun.lock" | "bun.lockb" => "bun",
                "pnpm-lock.yaml" => "pnpm",
                "yarn.lock" => "yarn",
                _ => "npm",
            })
            .unwrap_or("npm");
        let checks: Vec<String> = CHECK_SCRIPTS
            .iter()
            .filter(|name| {
                scripts[**name]
                    .as_str()
                    .is_some_and(|script| !script.contains("no test specified"))
            })
            .map(|name| format!("{runner} run {name}"))
            .collect();
        if !checks.is_empty() {
            for check in &checks {
                sources.push(Source {
                    from: "package.json".into(),
                    gave: format!("check `{check}`"),
                });
            }
            return checks;
        }
    }
    if root.join("Cargo.toml").is_file() {
        let check = if root.join("Cargo.lock").is_file() {
            "cargo test --locked"
        } else {
            "cargo test"
        };
        sources.push(Source {
            from: "Cargo.toml".into(),
            gave: format!("check `{check}`"),
        });
        return vec![check.into()];
    }
    Vec::new()
}

/// A `just` recipe named `check`: `check:` or `check arg:`, not an
/// assignment (`check := …`).
fn is_check_recipe(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("check") else {
        return false;
    };
    if rest.starts_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_') {
        return false;
    }
    match rest.find(':') {
        Some(at) => !rest[at..].starts_with(":="),
        None => false,
    }
}

/// The root and its immediate subdirectories, as paths relative to the root.
fn lockfile_dirs(root: &Path) -> Vec<String> {
    let mut dirs = vec![String::new()];
    let Ok(read) = std::fs::read_dir(root) else {
        return dirs;
    };
    let mut children: Vec<String> = read
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_str()))
        .filter(|name| {
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        .collect();
    children.sort();
    dirs.extend(children);
    dirs
}

fn join(dir: &str, file: &str) -> String {
    match dir {
        "" => file.to_string(),
        dir => format!("{dir}/{file}"),
    }
}

/// A path inside the repository with `.` and `..` resolved, or `None` for one
/// that leaves it.
fn normalize(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for part in path.components() {
        match part {
            std::path::Component::Normal(name) => parts.push(name.to_string_lossy().to_string()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// `devcontainer.json` is JSON with comments and trailing commas.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for next in chars.by_ref() {
                    if last == '*' && next == '/' {
                        break;
                    }
                    last = next;
                }
            }
            _ => out.push(c),
        }
    }
    // A comma before a closing bracket.
    let mut cleaned = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    let bytes: Vec<char> = out.chars().collect();
    for (at, &c) in bytes.iter().enumerate() {
        if in_string {
            cleaned.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        }
        if c == ',' {
            let next = bytes[at + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        cleaned.push(c);
    }
    cleaned
}

/// Draft an entry for `repo` from its default branch.
pub async fn draft(
    cfg: &Config,
    repo: &Path,
    approved: &[(String, String)],
) -> Result<Draft, String> {
    let (_, commit) = crate::repo_image::default_commit(repo).await?;
    let tree = crate::review::snapshot_candidate(
        &repo.to_string_lossy(),
        &commit,
        cfg.supervision.max_snapshot_bytes,
    )
    .await
    .map_err(|error| error.to_string())?;
    let repos = cfg.repos();
    let current = repos.iter().find(|entry| entry.matches(repo)).cloned();
    let (entry, sources, notes) = draft_from_tree(repo, &tree.root, current.as_ref(), approved);
    Ok(Draft {
        repo: repo.to_path_buf(),
        commit,
        entry,
        current,
        sources,
        notes,
    })
}

/// Write `entry` as the table's entry for `repo`, in place of the one that
/// matches it now or after the rest. Only an approved proposal calls this.
/// What the entry says about its sessions' egress is kept as it was.
pub fn save(cfg: &Config, repo: &Path, entry: Repo) -> Result<String, String> {
    let _edit = Config::edit_lock();
    let mut file = Config::try_load()?;
    let mut entries: Vec<Repo> = cfg.repos().as_ref().clone();
    let path = match entries.iter_mut().find(|current| current.matches(repo)) {
        Some(current) => {
            *current = Repo {
                path: current.path.clone(),
                session_egress: current.session_egress,
                ..entry
            };
            current.path.display().to_string()
        }
        None => {
            entries.push(Repo {
                path: repo.to_path_buf(),
                session_egress: false,
                ..entry
            });
            repo.display().to_string()
        }
    };
    crate::config::validate_repos(&entries)?;
    file.repo = entries.clone();
    file.save().map_err(|error| error.to_string())?;
    cfg.live_repo.set(entries);
    Ok(path)
}

/// One step of a trial.
#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// `prepare` or `check`.
    pub phase: &'static str,
    pub command: String,
    pub ok: bool,
    pub exit: Option<i32>,
    pub ms: u64,
    /// The end of what it printed.
    pub tail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trial {
    pub id: String,
    pub session_id: String,
    pub repo: PathBuf,
    /// `running`, `passed` or `failed`.
    pub state: &'static str,
    pub commit: Option<String>,
    pub image: Option<String>,
    pub image_source: Option<String>,
    pub entry: Repo,
    /// The hosts preparation could reach.
    pub egress_tried: Vec<String>,
    /// Hosts the entry names that this session cannot reach, so the trial
    /// prepared without them.
    pub egress_not_tried: Vec<String>,
    pub steps: Vec<Step>,
    /// Why the trial stopped before its steps, if it did.
    pub error: Option<String>,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
}

/// Trials, newest last per session. Held in memory: a trial is advice for
/// the session that asked, and a restart that drops one loses a report, not
/// a decision.
fn trials() -> &'static Mutex<HashMap<String, Trial>> {
    static TRIALS: std::sync::OnceLock<Mutex<HashMap<String, Trial>>> = std::sync::OnceLock::new();
    TRIALS.get_or_init(Default::default)
}

pub fn trial(id: &str) -> Option<Trial> {
    trials().lock().unwrap().get(id).cloned()
}

/// The trial a session has running, if any: one at a time.
pub fn running_for(session_id: &str) -> Option<Trial> {
    trials()
        .lock()
        .unwrap()
        .values()
        .find(|trial| trial.session_id == session_id && trial.state == "running")
        .cloned()
}

fn update(id: &str, change: impl FnOnce(&mut Trial)) {
    if let Some(trial) = trials().lock().unwrap().get_mut(id) {
        change(trial);
    }
}

/// Start trying `entry` for `repo` on behalf of `session_id`, with
/// preparation reaching only `reachable` of the hosts the entry names. The
/// trial runs on; read it back with [`trial`].
pub fn start(
    backend: Arc<dyn Backend>,
    cfg: Arc<Config>,
    store: Arc<Store>,
    session_id: &str,
    repo: PathBuf,
    entry: Repo,
    reachable: impl Fn(&str) -> bool,
) -> Result<Trial, String> {
    let hosts = entry.egress_hosts()?;
    let (egress_tried, egress_not_tried): (Vec<String>, Vec<String>) =
        hosts.into_iter().partition(|host| reachable(host));
    let id = uuid::Uuid::now_v7().simple().to_string();
    let trial = Trial {
        id: id.clone(),
        session_id: session_id.to_string(),
        repo: repo.clone(),
        state: "running",
        commit: None,
        image: None,
        image_source: None,
        entry: entry.clone(),
        egress_tried: egress_tried.clone(),
        egress_not_tried,
        steps: Vec::new(),
        error: None,
        started_ms: crate::store::now_ms(),
        finished_ms: None,
    };
    {
        let mut all = trials().lock().unwrap();
        // Keep the newest few per session.
        let mut mine: Vec<(i64, String)> = all
            .values()
            .filter(|old| old.session_id == session_id && old.state != "running")
            .map(|old| (old.started_ms, old.id.clone()))
            .collect();
        mine.sort();
        let excess = mine.len().saturating_sub(4);
        for (_, old) in mine.into_iter().take(excess) {
            all.remove(&old);
        }
        all.insert(id.clone(), trial.clone());
    }
    tokio::spawn(async move {
        let outcome = run(
            backend.as_ref(),
            &cfg,
            &store,
            &id,
            &repo,
            &entry,
            egress_tried,
        )
        .await;
        update(&id, |trial| {
            if let Err(error) = outcome {
                trial.error = Some(error);
            }
            let passed = trial.error.is_none() && trial.steps.iter().all(|step| step.ok);
            trial.state = if passed { "passed" } else { "failed" };
            trial.finished_ms = Some(crate::store::now_ms());
        });
    });
    Ok(trial)
}

/// Prepare the default branch the way the entry says, in a volume and a
/// cache of the trial's own, then run each check on a copy of the result
/// with the cache read-only and no egress — the shape a required check
/// runs in. Every volume is removed when it ends.
async fn run(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    id: &str,
    repo: &Path,
    entry: &Repo,
    egress: Vec<String>,
) -> Result<(), String> {
    let (_, commit) = crate::repo_image::default_commit(repo).await?;
    let mut environment = crate::environment::environment_with(cfg, store, Some(repo), Some(entry));
    environment.egress = egress;
    update(id, |trial| {
        trial.commit = Some(commit.clone());
        trial.image = Some(environment.image.clone());
        trial.image_source = Some(environment.image_source.to_string());
    });
    let tree = crate::review::snapshot_candidate(
        &repo.to_string_lossy(),
        &commit,
        cfg.supervision.max_snapshot_bytes,
    )
    .await
    .map_err(|error| error.to_string())?;
    let work = format!("tracon-try-{id}");
    let cache = format!("tracon-try-cache-{id}");
    let runner = backend.runner(Vec::new());
    let timeout = Duration::from_secs(environment.timeout_secs);
    let outcome = async {
        backend
            .import_writable(&work, &tree.root)
            .await
            .map_err(|error| format!("could not stage the trial: {error}"))?;
        // A cold cache is a slower trial, not a failed one.
        let _ = backend
            .clone_volume(&environment.cache_volume, &cache)
            .await;
        if !environment.prepare.is_empty() {
            let started = std::time::Instant::now();
            let prepared = crate::review::checks::run_preparation(
                backend,
                runner.as_ref(),
                &environment,
                (&work, &cache),
                &format!("tracon-t-{}", &id[..12]),
                tokio::time::Instant::now() + timeout,
                &crate::review::checks::RunToCompletion,
            )
            .await;
            let step = Step {
                phase: "prepare",
                command: environment.prepare.join(" && "),
                ok: prepared.is_ok(),
                exit: None,
                ms: started.elapsed().as_millis() as u64,
                tail: prepared
                    .as_ref()
                    .err()
                    .map(|stopped| tail(&stopped.to_string()))
                    .unwrap_or_default(),
            };
            let ok = step.ok;
            update(id, |trial| trial.steps.push(step));
            if !ok {
                return Ok(());
            }
        }
        for (index, command) in environment.checks.iter().enumerate() {
            let copy = format!("tracon-try-{id}-c{index}");
            let started = std::time::Instant::now();
            let step = match backend.clone_volume(&work, &copy).await {
                Err(error) => Step {
                    phase: "check",
                    command: command.clone(),
                    ok: false,
                    exit: None,
                    ms: 0,
                    tail: format!("could not stage the check: {error}"),
                },
                Ok(()) => {
                    let mut env = crate::environment::cache_env();
                    env.push(("CARGO_NET_OFFLINE".into(), "true".into()));
                    let name = format!("tracon-t-{}-c{index}", &id[..12]);
                    let running = runner.capture_name(&name);
                    let cmd = RunnerCommand {
                        argv: crate::review::output::merged_shell(command),
                        env,
                        mounts: vec![
                            Mount::volume(copy.clone(), "/work", false),
                            Mount::volume(cache.clone(), "/cache", true),
                        ],
                        workdir: Some("/work".into()),
                        name,
                        image: Some(environment.image.clone()),
                        expose: None,
                    };
                    let finished = tokio::time::timeout(timeout, runner.run_capture(cmd)).await;
                    let step = match finished {
                        Err(_) => {
                            let _ = runner.kill(&running).await;
                            Step {
                                phase: "check",
                                command: command.clone(),
                                ok: false,
                                exit: None,
                                ms: started.elapsed().as_millis() as u64,
                                tail: format!(
                                    "timed out after {}s; set `timeout_secs` if it needs longer",
                                    timeout.as_secs()
                                ),
                            }
                        }
                        Ok(Err(error)) => Step {
                            phase: "check",
                            command: command.clone(),
                            ok: false,
                            exit: None,
                            ms: started.elapsed().as_millis() as u64,
                            tail: format!("could not run: {error}"),
                        },
                        Ok(Ok(output)) => {
                            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                            text.push_str(&String::from_utf8_lossy(&output.stderr));
                            let ok = output.status.success();
                            let failures = if ok {
                                String::new()
                            } else {
                                crate::review::output::failure_lines(&text)
                            };
                            Step {
                                phase: "check",
                                command: command.clone(),
                                ok,
                                exit: output.status.code(),
                                ms: started.elapsed().as_millis() as u64,
                                tail: crate::review::output::with_failures(&failures, &tail(&text)),
                            }
                        }
                    };
                    let _ = backend.remove_volume(&copy).await;
                    step
                }
            };
            update(id, |trial| trial.steps.push(step));
        }
        Ok(())
    }
    .await;
    let _ = backend.remove_volume(&work).await;
    let _ = backend.remove_volume(&cache).await;
    outcome
}

/// The last lines of what a step printed: enough to see why it failed.
fn tail(text: &str) -> String {
    const MAX: usize = 3000;
    let text = text.trim_end();
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut start = text.len() - MAX;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_draft_reads_what_the_repository_already_says() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            ".devcontainer/devcontainer.json",
            r#"{
                // the dev image
                "build": { "dockerfile": "Dockerfile", "context": ".." },
                "postCreateCommand": "bun install", /* never run */
                "customizations": { "url": "http://x//y" },
            }"#,
        );
        write(root, ".devcontainer/Dockerfile", "FROM debian\n");
        write(root, "Cargo.lock", "");
        write(root, "Cargo.toml", "[package]\n");
        write(root, "spa/bun.lock", "");
        write(root, "spa/package-lock.json", "");
        write(root, "node_modules/x/yarn.lock", "");
        write(
            root,
            "justfile",
            "set shell := [\"bash\"]\ncheck_dir := \"x\"\ncheck: lint\n    cargo test\n",
        );
        let (entry, sources, notes) = draft_from_tree(
            Path::new("/src/app"),
            root,
            None,
            &[
                ("example.test".into(), "a1".into()),
                ("crates.io".into(), "a2".into()),
            ],
        );
        assert_eq!(entry.path, Path::new("/src/app"));
        assert_eq!(
            entry.dockerfile.as_deref(),
            Some(".devcontainer/Dockerfile")
        );
        assert_eq!(entry.context.as_deref(), Some("."));
        assert_eq!(
            entry.prepare,
            [
                "cargo fetch --locked",
                "cd spa && bun install --frozen-lockfile --ignore-scripts"
            ]
        );
        // A preset's own hosts are not repeated; a host the operator opened
        // to the session is suggested, with the approval that opened it.
        assert_eq!(entry.egress, ["crates", "npm", "example.test"]);
        assert!(sources.iter().any(|s| s.from == "approval a1"));
        assert_eq!(
            entry.checks.as_deref(),
            Some(&["just check".to_string()][..])
        );
        assert!(!entry.session_egress);
        assert!(
            notes.iter().any(|n| n.contains("postCreateCommand")),
            "{notes:?}"
        );
    }

    #[test]
    fn package_scripts_are_checks_when_there_is_no_recipe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "package.json",
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1","lint":"eslint .","build":"x","check":"tsc"}}"#,
        );
        write(root, "pnpm-lock.yaml", "");
        write(
            root,
            ".devcontainer.json",
            r#"{"image":"node:22","dockerComposeFile":"c.yml"}"#,
        );
        let current = Repo {
            path: "github.com/o/r".into(),
            session_egress: true,
            ..Default::default()
        };
        let (entry, _, notes) = draft_from_tree(Path::new("/x"), root, Some(&current), &[]);
        assert_eq!(entry.checks.unwrap(), ["pnpm run check", "pnpm run lint"]);
        // The entry it would replace keeps its path and its sessions' egress.
        assert_eq!(entry.path, Path::new("github.com/o/r"));
        assert!(entry.session_egress);
        assert!(entry.image.is_none());
        assert!(notes.iter().any(|n| n.contains("by tag")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("compose")), "{notes:?}");
    }

    #[test]
    fn an_empty_repository_says_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (entry, sources, notes) = draft_from_tree(Path::new("/x"), dir.path(), None, &[]);
        assert!(sources.is_empty());
        assert_eq!(entry.checks.as_deref(), Some(&[][..]));
        assert!(notes.iter().any(|n| n.contains("no check")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("no lockfile")), "{notes:?}");
    }

    #[test]
    fn a_check_recipe_is_not_an_assignment() {
        assert!(is_check_recipe("check:"));
        assert!(is_check_recipe("check *args: lint"));
        assert!(!is_check_recipe("check := 1"));
        assert!(!is_check_recipe("checks:"));
        assert!(!is_check_recipe("check-all:"));
    }

    #[test]
    fn a_path_cannot_leave_the_repository() {
        assert_eq!(
            normalize(Path::new(".devcontainer/../Dockerfile")).unwrap(),
            "Dockerfile"
        );
        assert!(normalize(Path::new("../x")).is_none());
        assert!(normalize(Path::new("/etc/x")).is_none());
    }
}
