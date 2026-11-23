//! `provider_cli_run`: a provider's command-line tool, run on the node's
//! host for an agent that never holds its credential.
//!
//! The operator names each tool in `[[provider_cli]]`: the program (held to
//! the path guard's rules for a program the node runs, checked again before
//! every spawn), the broker credential whose variables are its environment,
//! and the argument prefixes a run may begin with. The agent names only the
//! tool and its arguments.
//!
//! A run whose arguments begin with one of those prefixes is decided by the
//! signed policy and its grants, as any tool call is, with the tool's name
//! and its arguments as one line (`cli`, `command`) for a rule to scope. Any
//! other run is put to the operator, and a signed denial holds either way.
//!
//! The credential reaches the program as environment only: an argument that
//! carries one of its values is refused, and so is a credential variable
//! that would replace what the node sets itself (`PATH`, `HOME`). The
//! program starts with no other environment of the node's, in an empty
//! directory of its own that is also its `HOME`, so it reads no operator
//! profile beside the credential it was lent.
//!
//! What it prints is bounded and redacted before anyone sees it: every
//! credential value is replaced, then every line that looks like a secret.
//! Every run is recorded on the caller's log (`provider_cli_ran`): the
//! arguments, the exit status, and a digest of everything each stream
//! printed, so what was shown can be told apart from what was not.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::config::{Config, ProviderCli};
use crate::policy::{Decision, Verdict};

use super::{CallContext, SessionAccess};

pub const RUN: &str = "provider_cli_run";

/// The most arguments one run may take.
const MAX_ARGS: usize = 128;
/// The longest one argument may be.
const MAX_ARG_LEN: usize = 4096;
/// How much of each stream is kept to redact and show from. The rest is
/// read, counted and digested, never kept.
const CAPTURE_MAX: usize = 1 << 20;
/// How much of each stream the caller is shown, after redaction.
pub const SHOWN_MAX: usize = 32 * 1024;
/// What stands in for a credential's value in what is shown.
pub const REDACTED: &str = "[redacted credential]";
/// The environment the node sets itself, which a credential does not
/// replace.
const NODE_SET: &[&str] = &["PATH", "HOME", "TMPDIR"];
/// Credential values shorter than this are not searched for in what a run
/// prints or in its arguments: a value of a character or two would redact
/// half of every line and refuse every argument that contains it.
const MIN_SECRET_LEN: usize = 4;

/// The tool, offered for the `[[provider_cli]]` entries a channel may use.
pub fn definitions(cfg: &Config, channel: &str, available: &[&str]) -> Vec<Value> {
    let offered: Vec<&ProviderCli> = cfg
        .provider_cli
        .iter()
        .filter(|cli| cli.serves(channel) && lendable(cli, available))
        .collect();
    if offered.is_empty() {
        return Vec::new();
    }
    let names: Vec<&str> = offered.iter().map(|cli| cli.name.as_str()).collect();
    let described: Vec<String> = offered
        .iter()
        .map(|cli| match cli.allow.is_empty() {
            true => format!("{} (every run asked)", cli.name),
            false => format!("{} (follows policy: {})", cli.name, cli.allow.join(", ")),
        })
        .collect();
    definitions_for(&names, &described.join("; "))
}

/// The tool for `names`. With none, `cli` is any text: that is the schema an
/// operator's edit of a held call is checked against.
pub fn definitions_for(names: &[&str], offered: &str) -> Vec<Value> {
    let mut cli = json!({
        "type": "string",
        "description": "The provider CLI's name, as the node's configuration names it.",
    });
    if !names.is_empty() {
        cli["enum"] = json!(names);
    }
    let offered = match offered.is_empty() {
        true => String::new(),
        false => format!(" Offered here: {offered}."),
    };
    vec![json!({
        "name": RUN,
        "description": format!(
            "Run a provider's command-line tool on the node, with its credential, which you \
             never see. Give the arguments as a list, without the program's name: no shell \
             reads them, so quoting, pipes and variables mean nothing. A run whose arguments \
             begin with one of the operator's prefixes follows the node's policy; any other \
             run is put to the operator and answered once they decide. Output is cut at {} \
             KiB a stream, and credentials and secret-looking lines in it are redacted.{offered}",
            SHOWN_MAX / 1024
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "cli": cli,
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": MAX_ARGS,
                    "description": "The arguments, one element each, e.g. [\"s3\", \"ls\", \"s3://bucket\"].",
                },
            },
            "required": ["cli", "args"],
        },
    })]
}

/// Whether the channel may be lent the entry's credential, if it names one.
fn lendable(cli: &ProviderCli, available: &[&str]) -> bool {
    cli.credential
        .as_deref()
        .is_none_or(|credential| available.contains(&credential))
}

/// The arguments a call names, checked for shape.
pub fn argv(args: &Value) -> Result<Vec<String>, String> {
    let list = args
        .get("args")
        .and_then(Value::as_array)
        .ok_or("args is required: a list of strings")?;
    if list.len() > MAX_ARGS {
        return Err(format!(
            "args has {} entries; at most {MAX_ARGS}",
            list.len()
        ));
    }
    list.iter()
        .map(|arg| {
            let arg = arg.as_str().ok_or("every entry of args is a string")?;
            if arg.len() > MAX_ARG_LEN {
                return Err(format!("an argument is longer than {MAX_ARG_LEN} bytes"));
            }
            if arg.contains('\0') {
                return Err("an argument contains a NUL byte".to_string());
            }
            Ok(arg.to_string())
        })
        .collect()
}

/// The entry a call names, offered on the caller's channel.
pub fn entry<'a>(
    cfg: &'a Config,
    ctx: &CallContext,
    args: &Value,
) -> Result<&'a ProviderCli, String> {
    let name = args
        .get("cli")
        .and_then(Value::as_str)
        .ok_or("cli is required")?;
    let cli = cfg
        .provider_cli
        .iter()
        .find(|cli| cli.name == name)
        .ok_or_else(|| format!("no provider CLI named {name} is configured on this node"))?;
    if !cli.serves(&ctx.channel) {
        return Err(format!(
            "provider CLI {name} is not offered on channel {}",
            ctx.channel
        ));
    }
    Ok(cli)
}

/// Why a call cannot be asked or run as it stands: an entry the channel is
/// not offered, or arguments that are not a list of strings.
pub fn refusal(cfg: &Config, ctx: &CallContext, args: &Value) -> Option<String> {
    entry(cfg, ctx, args)
        .and_then(|_| argv(args))
        .err()
        .map(|why| format!("{why}; nothing was asked or run"))
}

/// A call's arguments as the policy matches them: the tool's name and its
/// arguments as one line, so an allow rule can scope `cli` and `command`
/// with globs and a denial's substrings see every word.
pub fn policy_arguments(args: &Value) -> Value {
    let command = argv(args).map(|argv| argv.join(" ")).unwrap_or_default();
    json!({
        "cli": args.get("cli").cloned().unwrap_or(Value::Null),
        "command": command,
    })
}

/// How a run is decided, from the signed policy's decision for it: a denial
/// holds; arguments that begin with none of the entry's prefixes are asked;
/// arguments that begin with one are the policy's (and its grants') to
/// decide.
pub fn decide(cli: &ProviderCli, argv: &[String], policy_decision: Decision) -> Decision {
    if policy_decision.verdict == Verdict::Deny {
        return policy_decision;
    }
    match cli.allowed_prefix(argv) {
        Some(_) => policy_decision,
        None => Decision {
            verdict: Verdict::Ask,
            rule_id: Some(format!("provider_cli.{}.allow", cli.name)),
            reason: Some(format!(
                "the arguments begin with none of the prefixes the operator lets provider CLI \
                 {} run under the policy; the operator decides each such run",
                cli.name
            )),
        },
    }
}

/// One stream of a run, as it was read.
#[derive(Debug, Default)]
pub struct Stream {
    /// The first [`CAPTURE_MAX`] bytes.
    pub kept: Vec<u8>,
    /// Every byte it printed.
    pub bytes: u64,
    /// SHA-256 of every byte it printed, in hex.
    pub sha256: String,
}

/// What a run did.
#[derive(Debug)]
pub struct Ran {
    /// The exit code; `None` when it was killed by a signal or by the timeout.
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub stdout: Stream,
    pub stderr: Stream,
}

async fn read_stream(mut from: impl AsyncRead + Unpin) -> Stream {
    let mut hasher = Sha256::new();
    let mut kept = Vec::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                bytes += n as u64;
                let room = CAPTURE_MAX.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    Stream {
        kept,
        bytes,
        sha256: hex::encode(hasher.finalize()),
    }
}

/// Run `program` with `argv` and `env` (and nothing else of the node's), in
/// an empty directory of its own, killed at `timeout`.
pub async fn run(
    program: &std::path::Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Ran, String> {
    let home = tempfile::Builder::new()
        .prefix("tracon-provider-cli-")
        .tempdir()
        .map_err(|e| format!("could not make the run's directory: {e}"))?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(argv)
        .env_clear()
        .envs(
            env.iter()
                .filter(|(key, _)| !NODE_SET.contains(&key.as_str())),
        )
        .env("PATH", crate::config::path_guard::child_path())
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .current_dir(home.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Ok(lang) = std::env::var("LANG") {
        command.env("LANG", lang);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", program.display()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let readers = async { tokio::join!(read_stream(stdout), read_stream(stderr)) };
    let waited = async {
        let streams = readers.await;
        (streams, child.wait().await)
    };
    match tokio::time::timeout(timeout, waited).await {
        Ok(((stdout, stderr), status)) => {
            let status = status.map_err(|e| format!("waiting for {}: {e}", program.display()))?;
            Ok(Ran {
                exit: status.code(),
                timed_out: false,
                stdout,
                stderr,
            })
        }
        // The child is killed as the future that holds it is dropped.
        Err(_) => Ok(Ran {
            exit: None,
            timed_out: true,
            stdout: Stream::default(),
            stderr: Stream::default(),
        }),
    }
}

/// The values a run's output and arguments are searched for, longest
/// first, so a value that holds another is replaced whole.
fn secrets_of(env: &BTreeMap<String, String>) -> Vec<&str> {
    let mut secrets: Vec<&str> = env
        .values()
        .map(String::as_str)
        .filter(|v| v.len() >= MIN_SECRET_LEN)
        .collect();
    secrets.sort_by_key(|v| std::cmp::Reverse(v.len()));
    secrets.dedup();
    secrets
}

/// One stream as the caller is shown it: every credential value replaced,
/// then every secret-looking line, then cut at [`SHOWN_MAX`]. A stream cut
/// at [`CAPTURE_MAX`] loses its last bytes first, which could hold the
/// beginning of a value no search would find whole.
pub fn shown(stream: &Stream, secrets: &[&str]) -> (String, bool) {
    let mut kept: &[u8] = &stream.kept;
    if stream.bytes > kept.len() as u64 {
        let longest = secrets.first().map_or(0, |s| s.len());
        kept = &kept[..kept.len().saturating_sub(longest)];
    }
    let mut text = String::from_utf8_lossy(kept).into_owned();
    for secret in secrets {
        text = text.replace(secret, REDACTED);
    }
    let text = super::operator::scrub(&text);
    let cut = text.len() > SHOWN_MAX || stream.bytes > stream.kept.len() as u64;
    let text = match text.len() > SHOWN_MAX {
        true => {
            let mut end = SHOWN_MAX;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}\n… (cut; {} bytes in all)", &text[..end], stream.bytes)
        }
        false if cut => format!("{text}\n… (cut; {} bytes in all)", stream.bytes),
        false => text,
    };
    (text, cut)
}

/// The arguments as the record keeps them: secret-looking ones redacted
/// as an audit record's are.
fn recorded_argv(argv: &[String]) -> Value {
    super::audit::redacted(&json!(argv))
}

/// Run an allowed call: the entry is read again, its program checked again
/// and its credential lent again, now. `approval` is the operator's card
/// that allowed it, when one did.
pub async fn call(
    tools: &super::Tools,
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
    approval: Option<&str>,
) -> Result<Value, String> {
    let cfg = tools.cfg.live();
    let cli = entry(&cfg, ctx, args)?.clone();
    let argv = argv(args)?;
    let env = match &cli.credential {
        None => BTreeMap::new(),
        Some(name) => {
            let broker = tools.broker.read().unwrap();
            if broker
                .get(name)
                .is_some_and(|c| c.kind != crate::broker::KIND_ENV)
            {
                return Err(format!(
                    "nothing ran: credential {name} is a model sign-in, not an env credential a \
                     provider CLI is lent"
                ));
            }
            broker
                .env_for(name, &ctx.channel, &ctx.node_id)
                .map_err(|e| format!("nothing ran: {e}"))?
        }
    };
    let secrets = secrets_of(&env);
    if argv
        .iter()
        .any(|arg| secrets.iter().any(|secret| arg.contains(secret)))
    {
        return Err(format!(
            "nothing ran: an argument carries the value of credential {}, which reaches the \
             program as environment only",
            cli.credential.as_deref().unwrap_or_default()
        ));
    }
    let program = crate::config::path_guard::program(
        crate::config::path_guard::PROVIDER_CLI_KEY,
        &cli.binary,
    )
    .map_err(|e| format!("nothing ran: {e}"))?;
    let started = std::time::Instant::now();
    let ran = run(
        &program,
        &argv,
        &env,
        Duration::from_secs(cli.timeout_secs()),
    )
    .await;
    let ms = started.elapsed().as_millis() as u64;
    let mut record = json!({
        "cli": cli.name,
        "argv": recorded_argv(&argv),
        "credential": cli.credential,
        "ms": ms,
    });
    if let Some(prefix) = cli.allowed_prefix(&argv) {
        record["prefix"] = json!(prefix);
    }
    if let Some(approval) = approval {
        record["approval_id"] = json!(approval);
    }
    let ran = match ran {
        Ok(ran) => ran,
        Err(error) => {
            record["error"] = json!(error);
            access.manager.record_for(
                ctx,
                crate::session::state::event_kind::PROVIDER_CLI_RAN,
                approval,
                record,
            );
            return Err(error);
        }
    };
    let (stdout, stdout_cut) = shown(&ran.stdout, &secrets);
    let (stderr, stderr_cut) = shown(&ran.stderr, &secrets);
    record["exit"] = json!(ran.exit);
    record["timed_out"] = json!(ran.timed_out);
    record["stdout"] = json!({ "bytes": ran.stdout.bytes, "sha256": ran.stdout.sha256 });
    record["stderr"] = json!({ "bytes": ran.stderr.bytes, "sha256": ran.stderr.sha256 });
    access.manager.record_for(
        ctx,
        crate::session::state::event_kind::PROVIDER_CLI_RAN,
        approval,
        record,
    );
    if ran.timed_out {
        return Err(format!(
            "{} was stopped after {}s without finishing; what it did before then is not known",
            cli.name,
            cli.timeout_secs()
        ));
    }
    Ok(json!({
        "cli": cli.name,
        "exit": ran.exit,
        "ok": ran.exit == Some(0),
        "stdout": stdout,
        "stderr": stderr,
        "stdout_cut": stdout_cut,
        "stderr_cut": stderr_cut,
        "stdout_sha256": ran.stdout.sha256,
        "stderr_sha256": ran.stderr.sha256,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(allow: &[&str]) -> ProviderCli {
        ProviderCli {
            name: "aws".into(),
            binary: "aws".into(),
            credential: Some("aws-prod".into()),
            channels: vec!["ops".into()],
            allow: allow.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 0,
        }
    }

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn decision(verdict: Verdict) -> Decision {
        Decision {
            verdict,
            rule_id: Some("rule".into()),
            reason: None,
        }
    }

    #[test]
    fn a_run_under_an_allowed_prefix_follows_the_policy_and_any_other_is_asked() {
        let cli = sample(&["s3 ls", "cloudfront create-invalidation"]);
        let listed = words("s3 ls s3://bucket");
        for verdict in [Verdict::Allow, Verdict::Ask, Verdict::Deny] {
            assert_eq!(decide(&cli, &listed, decision(verdict)).verdict, verdict);
        }
        // Word by word: `s3 lsx` and `s3` alone begin with no prefix.
        for other in ["s3 rm s3://bucket/x", "s3 lsx", "s3", "", "ls s3"] {
            let got = decide(&cli, &words(other), decision(Verdict::Allow));
            assert_eq!(got.verdict, Verdict::Ask, "{other}");
            assert_eq!(got.rule_id.as_deref(), Some("provider_cli.aws.allow"));
        }
        // A denial holds whatever the arguments.
        let got = decide(&cli, &words("s3 rm x"), decision(Verdict::Deny));
        assert_eq!(got.verdict, Verdict::Deny);
        // No prefixes: every run asked.
        assert_eq!(
            decide(&sample(&[]), &listed, decision(Verdict::Allow)).verdict,
            Verdict::Ask
        );
    }

    #[test]
    fn the_policy_sees_the_tool_and_its_arguments_as_one_line() {
        let args = json!({ "cli": "aws", "args": ["s3", "ls", "s3://b"] });
        assert_eq!(
            policy_arguments(&args),
            json!({ "cli": "aws", "command": "s3 ls s3://b" })
        );
    }

    #[test]
    fn arguments_are_a_bounded_list_of_strings() {
        assert_eq!(
            argv(&json!({ "args": ["a", "b"] })).unwrap(),
            vec!["a", "b"]
        );
        assert!(argv(&json!({ "args": "s3 ls" })).is_err());
        assert!(argv(&json!({ "args": [1] })).is_err());
        assert!(argv(&json!({ "args": ["a\0b"] })).is_err());
        assert!(argv(&json!({ "args": vec!["x"; MAX_ARGS + 1] })).is_err());
        assert!(argv(&json!({ "args": ["x".repeat(MAX_ARG_LEN + 1)] })).is_err());
    }

    #[test]
    fn output_is_redacted_of_the_credential_and_secret_lines_and_bounded() {
        let secrets = vec!["AKIASECRETVALUE"];
        let text = "listing\nkey AKIASECRETVALUE here\npassword=hunter2\nend\n";
        let stream = Stream {
            kept: text.as_bytes().to_vec(),
            bytes: text.len() as u64,
            sha256: String::new(),
        };
        let (shown, cut) = shown(&stream, &secrets);
        assert!(!cut);
        assert!(!shown.contains("AKIASECRETVALUE"), "{shown}");
        assert!(shown.contains(REDACTED), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(
            shown.contains("listing") && shown.contains("end"),
            "{shown}"
        );

        let long = "word ".repeat(SHOWN_MAX);
        let stream = Stream {
            kept: long.as_bytes().to_vec(),
            bytes: long.len() as u64,
            sha256: String::new(),
        };
        let (shown, cut) = super::shown(&stream, &secrets);
        assert!(cut);
        assert!(shown.len() < SHOWN_MAX + 100, "{}", shown.len());
        assert!(shown.ends_with(&format!("(cut; {} bytes in all)", long.len())));
    }

    #[test]
    fn a_capture_cut_mid_credential_shows_none_of_it() {
        // What was kept ends partway into the value; the rest was never kept.
        let secrets = vec!["AKIASECRETVALUE"];
        let kept = b"ok\nAKIASEC".to_vec();
        let stream = Stream {
            bytes: kept.len() as u64 + 100,
            kept,
            sha256: String::new(),
        };
        let (shown, cut) = shown(&stream, &secrets);
        assert!(cut);
        assert!(!shown.contains("AKIASEC"), "{shown}");
    }

    #[tokio::test]
    async fn a_run_gets_its_credential_as_environment_and_nothing_else_of_the_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("fake-cli");
        std::fs::write(
            &program,
            "#!/bin/sh\necho \"args: $*\"\necho \"key: $FAKE_KEY\"\necho \"node: ${CARGO:-none}\"\npwd\necho oops >&2\nexit 3\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = BTreeMap::from([
            ("FAKE_KEY".to_string(), "sekritvalue".to_string()),
            ("HOME".to_string(), "/somewhere/else".to_string()),
        ]);
        let ran = run(&program, &words("s3 ls"), &env, Duration::from_secs(20))
            .await
            .unwrap();
        assert_eq!(ran.exit, Some(3));
        assert!(!ran.timed_out);
        let out = String::from_utf8_lossy(&ran.stdout.kept).into_owned();
        assert!(out.contains("args: s3 ls"), "{out}");
        assert!(out.contains("key: sekritvalue"), "{out}");
        assert!(out.contains("node: none"), "{out}");
        // Its own directory, not the HOME the credential named.
        assert!(out.contains("tracon-provider-cli-"), "{out}");
        assert_eq!(ran.stdout.bytes, out.len() as u64);
        assert_eq!(
            ran.stdout.sha256,
            hex::encode(Sha256::digest(out.as_bytes()))
        );
        assert_eq!(String::from_utf8_lossy(&ran.stderr.kept), "oops\n");
        let (shown, _) = shown(&ran.stdout, &secrets_of(&env));
        assert!(!shown.contains("sekritvalue"), "{shown}");
    }

    #[tokio::test]
    async fn a_run_past_its_timeout_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("slow-cli");
        std::fs::write(&program, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let ran = run(&program, &[], &BTreeMap::new(), Duration::from_millis(300))
            .await
            .unwrap();
        assert!(ran.timed_out);
        assert_eq!(ran.exit, None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
