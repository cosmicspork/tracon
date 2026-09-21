//! The command deployment kind: one operator-configured argv, run on the node
//! with a brokered credential supplied as environment, plus the argv that
//! observe what it produced.
//!
//! Three properties hold by construction here rather than by each call site
//! remembering them:
//!
//! - **The credential is environment and only environment.** It is fetched
//!   from the broker under the candidate's own channel and this node, injected
//!   at spawn, and never written into argv, the recorded tail, or the row.
//!   Configuration that puts a credential-shaped word in argv is refused
//!   before it ever runs (`config::validate_argv`), and every value the broker
//!   handed over is scrubbed out of output before it becomes evidence.
//! - **There is no shell.** The argv is the command. No string is ever parsed
//!   into words, `sh -c` and the interpreters are refused as the binary, and
//!   the environment is cleared rather than inherited, so a deploy cannot pick
//!   up whatever the operator's own login left behind.
//! - **Substitution is total.** A `{placeholder}` that is not known is a
//!   configuration error, so an unsubstituted brace can never survive into a
//!   command line.
//!
//! Evidence identity is a digest rather than a pinned image, because there is
//! no image: a command kind's equivalent of `execution_image` is a hash over
//! the argv template, the environment key names, the resolved absolute path of
//! the binary, and what that binary says its version is. Upgrade the deploy
//! CLI and the identity changes, which is exactly what the pinned image buys
//! the GitLab kind.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{argv_placeholders, Config, QaDiscover, QaStatusCommand, QaTarget};

/// Output kept from one invocation, for parsing. The tail that becomes
/// evidence is bounded separately and much smaller.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// The tail of a deploy command's output that is recorded, after redaction.
pub const MAX_TAIL_BYTES: usize = 8 * 1024;
/// The version probe, which must never be the slow part of anything.
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// What a candidate contributes to the placeholders and environment of every
/// command this target runs.
#[derive(Debug, Clone)]
pub struct Subject {
    pub target_id: String,
    pub sha: String,
    pub branch: String,
}

impl Subject {
    fn bindings(&self, target: &QaTarget) -> BTreeMap<String, String> {
        let mut bindings = BTreeMap::new();
        bindings.insert("sha".into(), self.sha.clone());
        bindings.insert("short_sha".into(), short_sha(&self.sha));
        bindings.insert("branch".into(), self.branch.clone());
        bindings.insert("target".into(), self.target_id.clone());
        for (key, value) in &target.deployment.args {
            bindings.insert(key.clone(), value.clone());
        }
        bindings
    }

    /// The environment every command receives beside the credential. These are
    /// facts about the candidate, not secrets: a host CLI that prefers an
    /// environment variable to an argument can read them here.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("TRACON_CANDIDATE_SHA".into(), self.sha.clone()),
            ("TRACON_CANDIDATE_BRANCH".into(), self.branch.clone()),
            ("TRACON_QA_TARGET".into(), self.target_id.clone()),
        ]
    }
}

pub fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Substitute placeholders into one argv. `extra` carries the bindings that
/// exist only after discovery (`env_id`, `env_url`); an argv that names one
/// before it exists is a configuration error, not an empty string.
pub fn substitute(
    argv: &[String],
    target: &QaTarget,
    subject: &Subject,
    extra: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut bindings = subject.bindings(target);
    bindings.extend(extra.clone());
    argv.iter()
        .map(|part| {
            let mut out = part.clone();
            for name in argv_placeholders(part)? {
                let value = bindings
                    .get(&name)
                    .ok_or_else(|| format!("no value for placeholder {{{name}}}"))?;
                out = out.replace(&format!("{{{name}}}"), value);
            }
            if out.is_empty() || out.contains(['\0', '\r', '\n']) {
                return Err(format!(
                    "substituting {part:?} produced an empty or control-bearing argument"
                ));
            }
            Ok(out)
        })
        .collect()
}

/// One invocation's result, already redacted.
#[derive(Debug, Clone)]
pub struct Run {
    pub argv: Vec<String>,
    pub exit_status: Option<i32>,
    pub ok: bool,
    /// Stdout, bounded, redacted. Parsed as JSON by the observers.
    pub stdout: String,
    /// The tail of stdout and stderr together, bounded and redacted, for
    /// evidence. Never the whole of either.
    pub tail: String,
}

/// Run one argv on the node. `env` is the complete environment: nothing the
/// node inherited reaches it except `PATH`, and `HOME` is node-owned state so
/// a CLI's own configuration cannot be the operator's.
pub async fn run(
    argv: &[String],
    target_id: &str,
    env: &[(String, String)],
    secrets: &[String],
    timeout: Duration,
) -> Result<Run, String> {
    let binary = argv.first().ok_or("a deploy command needs a binary")?;
    let home = command_home(target_id);
    std::fs::create_dir_all(&home).map_err(|error| {
        format!("could not create the node-owned home for QA commands: {error}")
    })?;
    let mut command = tokio::process::Command::new(binary);
    command
        .args(&argv[1..])
        // Nothing the operator's own login left behind reaches a deploy
        // command: not their CLI configuration, not an ambient cloud token,
        // not a proxy setting. What it gets is PATH, a node-owned HOME, the
        // candidate's facts, and the brokered credential.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &home)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(format!(
                "could not run the configured QA command {binary:?}: {error}"
            ))
        }
        Err(_) => {
            return Err(format!(
                "the configured QA command {binary:?} did not finish within {}s",
                timeout.as_secs()
            ))
        }
    };
    let stdout = bounded(&output.stdout, MAX_OUTPUT_BYTES);
    let mut both = output.stdout.clone();
    both.extend_from_slice(&output.stderr);
    let tail = bounded(&both, MAX_TAIL_BYTES);
    Ok(Run {
        argv: argv.to_vec(),
        exit_status: output.status.code(),
        ok: output.status.success(),
        stdout: super::redact_secrets(&stdout, secrets.to_vec()),
        tail: super::redact_secrets(&tail, secrets.to_vec()),
    })
}

fn bounded(raw: &[u8], max: usize) -> String {
    let start = raw.len().saturating_sub(max);
    String::from_utf8_lossy(&raw[start..]).into_owned()
}

/// Where a command-kind target's commands see `$HOME`. Per target, under node
/// state, so one target's CLI configuration is not another's and neither is
/// the operator's own.
pub fn command_home(target_id: &str) -> PathBuf {
    Config::state_dir().join("qa-command-home").join(target_id)
}

/// A command kind's `execution_image`: the identity of the tool that did the
/// deploying. Same inputs, same digest; a new CLI version, a different binary
/// on PATH, a changed argv, or a changed set of injected environment names all
/// move it, so evidence recorded under one identity can never quietly be
/// evidence of another.
pub fn execution_identity(
    argv_template: &[String],
    env_names: &BTreeSet<String>,
    binary_path: &str,
    binary_version: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"tracon-qa-command-v1\n");
    for part in argv_template {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"--env--\n");
    for name in env_names {
        hasher.update(name.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"--binary--\n");
    hasher.update(binary_path.as_bytes());
    hasher.update(b"\n");
    hasher.update(binary_version.as_bytes());
    format!("command:sha256:{}", hex::encode(hasher.finalize()))
}

/// The absolute path a bare binary name resolves to on this node's `PATH`.
/// Recorded in the execution identity, so "the operator wrote `cloud`" becomes
/// "this exact file ran".
pub fn resolve_binary(binary: &str) -> Result<PathBuf, String> {
    if binary.starts_with('/') {
        let path = PathBuf::from(binary);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "configured QA deploy binary {binary} is not a file"
            ))
        };
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = PathBuf::from(dir).join(binary);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "configured QA deploy binary {binary:?} is not on this node's PATH"
    ))
}

/// What the deploy binary says it is. Failure is not fatal — a CLI without a
/// `--version` still deploys — but it is recorded as such rather than as an
/// empty string that would collide with every other silent tool.
pub async fn binary_version(
    binary: &str,
    target_id: &str,
    env: &[(String, String)],
    secrets: &[String],
) -> String {
    let argv = vec![binary.to_string(), "--version".into()];
    match run(&argv, target_id, env, secrets, VERSION_TIMEOUT).await {
        Ok(run) if run.ok => {
            let line = run.stdout.lines().next().unwrap_or_default().trim();
            if line.is_empty() {
                "version-not-reported".into()
            } else {
                line.chars().take(200).collect()
            }
        }
        Ok(_) => "version-command-failed".into(),
        Err(_) => "version-command-unavailable".into(),
    }
}

/// The environment a discovery selected, with only the fields tracon reads.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Discovered {
    pub id: String,
    pub url: String,
    pub branch: String,
    pub state: String,
}

/// Pick the host environment that belongs to this candidate's published
/// branch. Never the newest, never the only one: the branch match is the whole
/// binding, and an entry that does not carry the branch is not a candidate for
/// it. When `prefer_field` is configured, an entry the host's own automation
/// created wins over one a person made by hand on the same branch.
pub fn select_environment(
    stdout: &str,
    discover: &QaDiscover,
    branch: &str,
) -> Result<Discovered, String> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("discover command did not print JSON: {error}"))?;
    let rows = rows_of(&value, &discover.list_field)?;
    let mut matched: Vec<&Value> = rows
        .iter()
        .copied()
        .filter(|row| field_str(row, &discover.branch_field).as_deref() == Some(branch))
        .collect();
    if matched.is_empty() {
        return Err(format!("no environment for branch {branch}"));
    }
    if !discover.prefer_field.is_empty() {
        matched.sort_by_key(|row| !truthy(row.get(&discover.prefer_field)));
    }
    let chosen = matched[0];
    let id = field_str(chosen, &discover.id_field).ok_or_else(|| {
        format!(
            "the environment for branch {branch} has no {} field",
            discover.id_field
        )
    })?;
    let url = field_str(chosen, &discover.url_field).ok_or_else(|| {
        format!(
            "the environment for branch {branch} has no {} field",
            discover.url_field
        )
    })?;
    let state = field_str(chosen, &discover.state_field).unwrap_or_default();
    Ok(Discovered {
        id,
        url,
        branch: branch.to_string(),
        state,
    })
}

/// What a status command reported about the deployment it was asked about.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Reported {
    pub id: Option<String>,
    pub state: String,
    pub commit: Option<String>,
}

pub fn read_status(stdout: &str, status: &QaStatusCommand) -> Result<Reported, String> {
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("status command did not print JSON: {error}"))?;
    let row = if value.is_object() && status.list_field.is_empty() {
        value.clone()
    } else {
        rows_of(&value, &status.list_field)?
            .first()
            .map(|row| (*row).clone())
            .ok_or("status command reported no deployment")?
    };
    Ok(Reported {
        id: if status.id_field.is_empty() {
            None
        } else {
            field_str(&row, &status.id_field)
        },
        state: field_str(&row, &status.state_field).unwrap_or_default(),
        commit: if status.commit_field.is_empty() {
            None
        } else {
            field_str(&row, &status.commit_field)
        },
    })
}

fn rows_of<'a>(value: &'a Value, list_field: &str) -> Result<Vec<&'a Value>, String> {
    let array = if list_field.is_empty() {
        value.as_array()
    } else {
        value.get(list_field).and_then(Value::as_array)
    };
    array
        .map(|rows| rows.iter().collect())
        .ok_or_else(|| match list_field.is_empty() {
            true => "expected a JSON array on stdout".into(),
            false => format!("expected a JSON array under {list_field:?} on stdout"),
        })
}

/// Read a field as a string, accepting the number an id often is. Anything
/// else is absent rather than stringified: a host that printed an object where
/// a branch name belongs has not told us the branch.
fn field_str(row: &Value, field: &str) -> Option<String> {
    match row.get(field) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

fn truthy(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
        || matches!(value, Some(Value::String(text)) if text == "true")
}

/// Whether an observed identity attests this candidate. The endpoint may
/// report the full SHA or an abbreviation of it; it may also report a longer
/// build string that contains one. It may not report something merely similar:
/// a prefix shorter than seven characters is not an identification.
pub fn identity_attests(identity: &str, sha: &str) -> bool {
    let identity = identity.trim().to_ascii_lowercase();
    let sha = sha.trim().to_ascii_lowercase();
    if identity.is_empty() || sha.len() < 7 {
        return false;
    }
    if identity.contains(&sha) {
        return true;
    }
    // An abbreviation the host chose: any 7-or-more prefix of the candidate
    // SHA appearing in the identity, longest first so a coincidence of seven
    // characters is not preferred to a real match.
    (7..=sha.len())
        .rev()
        .any(|len| identity.contains(&sha[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{QaDeployment, QA_KIND_COMMAND};

    fn target() -> QaTarget {
        QaTarget {
            deployment: QaDeployment {
                kind: QA_KIND_COMMAND.into(),
                args: BTreeMap::from([("app".to_string(), "my-app".to_string())]),
                ..QaDeployment::default()
            },
            ..QaTarget::default()
        }
    }

    fn subject() -> Subject {
        Subject {
            target_id: "cloud-qa".into(),
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            branch: "feat/thing".into(),
        }
    }

    #[test]
    fn every_placeholder_including_the_operators_own_is_substituted() {
        let argv = vec![
            "cloud".to_string(),
            "deploy".into(),
            "{app}".into(),
            "qa-{short_sha}".into(),
            "--branch={branch}".into(),
        ];
        let out = substitute(&argv, &target(), &subject(), &BTreeMap::new()).unwrap();
        assert_eq!(
            out,
            vec![
                "cloud",
                "deploy",
                "my-app",
                "qa-0123456789ab",
                "--branch=feat/thing"
            ]
        );
    }

    #[test]
    fn a_placeholder_with_no_value_yet_is_an_error_rather_than_an_empty_argument() {
        let argv = vec!["cloud".to_string(), "{env_id}".into()];
        let error = substitute(&argv, &target(), &subject(), &BTreeMap::new()).unwrap_err();
        assert!(error.contains("env_id"), "{error}");
        let extra = BTreeMap::from([("env_id".to_string(), "env-7".to_string())]);
        assert_eq!(
            substitute(&argv, &target(), &subject(), &extra).unwrap(),
            vec!["cloud", "env-7"]
        );
    }

    #[test]
    fn the_automations_environment_wins_over_a_hand_made_one_on_the_same_branch() {
        let discover = QaDiscover {
            prefer_field: "createdFromAutomation".into(),
            ..QaDiscover::default()
        };
        let stdout = serde_json::json!([
            { "id": "by-hand", "url": "https://a.example.com", "branch": "feat/thing", "status": "running", "createdFromAutomation": false },
            { "id": "auto", "url": "https://b.example.com", "branch": "feat/thing", "status": "running", "createdFromAutomation": true },
            { "id": "other", "url": "https://c.example.com", "branch": "main", "status": "running", "createdFromAutomation": true },
        ])
        .to_string();
        let found = select_environment(&stdout, &discover, "feat/thing").unwrap();
        assert_eq!(found.id, "auto");
        assert_eq!(found.url, "https://b.example.com");
    }

    #[test]
    fn a_branch_with_no_environment_is_named_rather_than_guessed_at() {
        let stdout = serde_json::json!([{ "id": "1", "url": "https://a.example.com", "branch": "main", "status": "running" }])
            .to_string();
        let error = select_environment(&stdout, &QaDiscover::default(), "feat/thing").unwrap_err();
        assert_eq!(error, "no environment for branch feat/thing");
    }

    #[test]
    fn a_status_list_is_read_at_its_newest_entry_and_an_object_as_itself() {
        let status = QaStatusCommand {
            commit_field: "commitHash".into(),
            ..QaStatusCommand::default()
        };
        let listed = serde_json::json!([
            { "id": "d-2", "status": "success", "commitHash": "abc" },
            { "id": "d-1", "status": "failed", "commitHash": "old" },
        ])
        .to_string();
        assert_eq!(
            read_status(&listed, &status).unwrap(),
            Reported {
                id: Some("d-2".into()),
                state: "success".into(),
                commit: Some("abc".into())
            }
        );
        let single =
            serde_json::json!({ "id": 9, "status": "pending", "commitHash": "def" }).to_string();
        assert_eq!(
            read_status(&single, &status).unwrap(),
            Reported {
                id: Some("9".into()),
                state: "pending".into(),
                commit: Some("def".into())
            }
        );
    }

    #[test]
    fn an_identity_attests_a_candidate_only_by_a_real_abbreviation_of_its_sha() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert!(identity_attests(sha, sha));
        assert!(identity_attests("build 0123456789ab on qa", sha));
        assert!(identity_attests("0123456", sha));
        assert!(!identity_attests("012345", sha));
        assert!(!identity_attests("9999999999", sha));
        assert!(!identity_attests("", sha));
    }

    #[test]
    fn the_execution_identity_moves_when_the_tool_or_its_environment_does() {
        let argv = vec!["cloud".to_string(), "deploy".into()];
        let names = BTreeSet::from(["HOME".to_string(), "PATH".to_string()]);
        let base = execution_identity(&argv, &names, "/usr/bin/cloud", "0.5.0");
        assert!(base.starts_with("command:sha256:"), "{base}");
        assert_ne!(
            base,
            execution_identity(&argv, &names, "/usr/bin/cloud", "0.6.0")
        );
        assert_ne!(
            base,
            execution_identity(&argv, &names, "/opt/cloud", "0.5.0")
        );
        assert_ne!(
            base,
            execution_identity(
                &argv,
                &BTreeSet::from(["HOME".to_string()]),
                "/usr/bin/cloud",
                "0.5.0"
            )
        );
        assert_eq!(
            base,
            execution_identity(&argv, &names, "/usr/bin/cloud", "0.5.0")
        );
    }
}
