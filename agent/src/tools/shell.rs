//! `sh -c` with piped output, its tail streamed to the node while it runs. Not
//! a PTY: none has been needed yet (agent/README.md, Not built).

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{args, Context, Output, Progress, Tool};

const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 600;
/// Per stream. The tail is what matters: test failures and errors come last.
const KEEP: usize = 16 * 1024;
/// How often a running command's output so far is shown, and how much of it.
const LIVE_EVERY: Duration = Duration::from_secs(1);
const LIVE_KEEP: usize = 2 * 1024;

/// Both streams as they arrive, interleaved, for showing a running command;
/// `seen` counts what was added, so an unchanged tail is not shown again.
#[derive(Default)]
struct Live {
    bytes: Vec<u8>,
    seen: u64,
}

impl Live {
    fn push(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
        if self.bytes.len() > LIVE_KEEP {
            self.bytes.drain(..self.bytes.len() - LIVE_KEEP);
        }
        self.seen += b.len() as u64;
    }
}

pub struct Shell;

#[derive(Deserialize)]
struct Args {
    command: String,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a command with `sh -c` in the working directory, e.g. to run tests or git. \
         No input is available to it. Output is cut to its last 16 KiB per stream. \
         Use read, grep and glob rather than cat, grep and find."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_secs": {"type": "integer", "description": "Default 120, at most 600."}
            },
            "required": ["command"]
        })
    }

    async fn run(&self, cx: &Context, input: Value) -> Output {
        self.run_live(cx, input, &|_| {}).await
    }

    async fn run_live(&self, cx: &Context, input: Value, progress: Progress<'_>) -> Output {
        let a: Args = match args(input) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let timeout = a
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(1, MAX_TIMEOUT);
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&a.command)
            .current_dir(&cx.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own group, so a timeout kills what it started too.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Output::error(format!("could not start sh: {e}")),
        };
        let pid = child.id();
        let live = Arc::new(Mutex::new(Live::default()));
        let stdout = tokio::spawn(tail(child.stdout.take().expect("piped"), live.clone()));
        let stderr = tokio::spawn(tail(child.stderr.take().expect("piped"), live.clone()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
        let mut ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + LIVE_EVERY, LIVE_EVERY);
        let mut shown = 0;
        // `Err` is the timeout.
        let status = loop {
            tokio::select! {
                s = child.wait() => break Ok(s),
                _ = tokio::time::sleep_until(deadline) => break Err(()),
                _ = ticker.tick() => {
                    let now = {
                        let l = live.lock().unwrap_or_else(|e| e.into_inner());
                        (l.seen != shown).then(|| (l.seen, String::from_utf8_lossy(&l.bytes).into_owned()))
                    };
                    if let Some((seen, text)) = now {
                        shown = seen;
                        progress(&text);
                    }
                }
            }
        };
        let timed_out = status.is_err();
        if timed_out {
            if let Some(pid) = pid {
                // SAFETY: signalling a process group we created; no memory involved.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = child.wait().await;
        }
        let failed = !matches!(&status, Ok(Ok(s)) if s.success());
        let (out, out_total) = stdout.await.unwrap_or_default();
        let (err, err_total) = stderr.await.unwrap_or_default();
        let mut text = match status {
            Ok(Ok(s)) => match s.code() {
                Some(code) => format!("exit code {code}\n"),
                None => "killed by a signal\n".to_string(),
            },
            Ok(Err(e)) => format!("wait failed: {e}\n"),
            Err(_) => format!("timed out after {timeout}s and was killed\n"),
        };
        section(&mut text, "stdout", &out, out_total);
        section(&mut text, "stderr", &err, err_total);
        Output {
            content: text,
            is_error: failed,
        }
    }
}

fn section(text: &mut String, name: &str, kept: &[u8], total: usize) {
    if total == 0 {
        return;
    }
    text.push_str(&format!("--- {name}"));
    if total > kept.len() {
        text.push_str(&format!(" (last {} of {total} bytes)", kept.len()));
    }
    text.push_str(" ---\n");
    text.push_str(&String::from_utf8_lossy(kept));
    if !text.ends_with('\n') {
        text.push('\n');
    }
}

/// Reads to the end keeping only the last `KEEP` bytes; returns them and the
/// total read. What it reads also goes to `live`, for showing.
async fn tail(mut r: impl AsyncRead + Unpin, live: Arc<Mutex<Live>>) -> (Vec<u8>, usize) {
    let mut kept = Vec::new();
    let mut total = 0;
    let mut buf = [0u8; 8192];
    while let Ok(n) = r.read(&mut buf).await {
        if n == 0 {
            break;
        }
        total += n;
        kept.extend_from_slice(&buf[..n]);
        live.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(&buf[..n]);
        if kept.len() > KEEP {
            kept.drain(..kept.len() - KEEP);
        }
    }
    (kept, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> (tempfile::TempDir, Context) {
        let dir = tempfile::tempdir().unwrap();
        let cx = Context::new(dir.path()).unwrap();
        (dir, cx)
    }

    #[tokio::test]
    async fn runs_in_the_root_and_reports_exit() {
        let (_dir, cx) = cx();
        let out = Shell
            .run(&cx, json!({"command": "pwd; echo oops >&2; exit 3"}))
            .await;
        assert!(out.is_error);
        assert!(out.content.starts_with("exit code 3\n"), "{}", out.content);
        assert!(out.content.contains(&cx.root.display().to_string()));
        assert!(out.content.contains("--- stderr ---\noops\n"));
    }

    /// A running command shows what it has printed so far, once a second
    /// while that changes, and its result is unchanged by it.
    #[tokio::test]
    async fn a_running_command_shows_its_output_so_far() {
        let (_dir, cx) = cx();
        let seen = Mutex::new(Vec::<String>::new());
        let out = Shell
            .run_live(
                &cx,
                json!({"command": "echo one; sleep 1.5; echo two >&2; sleep 1.5; echo three"}),
                &|t| seen.lock().unwrap().push(t.to_string()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("one\nthree"), "{}", out.content);
        let seen = seen.into_inner().unwrap();
        assert!(seen.len() >= 2, "{seen:?}");
        assert_eq!(seen[0], "one\n");
        assert!(seen.iter().any(|t| t == "one\ntwo\n"), "{seen:?}");
        // Nothing new, nothing shown: no two in a row are the same.
        assert!(seen.windows(2).all(|w| w[0] != w[1]), "{seen:?}");
    }

    #[tokio::test]
    async fn keeps_the_tail() {
        let (_dir, cx) = cx();
        let out = Shell
            .run(
                &cx,
                json!({"command": "head -c 40000 /dev/zero | tr '\\0' a; echo END"}),
            )
            .await;
        assert!(!out.is_error);
        assert!(out
            .content
            .contains(&format!("(last {KEEP} of 40004 bytes)")));
        assert!(out.content.trim_end().ends_with("END"));
    }

    #[tokio::test]
    async fn times_out_and_kills_the_group() {
        let (_dir, cx) = cx();
        let started = std::time::Instant::now();
        let out = Shell
            .run(
                &cx,
                json!({"command": "sleep 30 & sleep 30", "timeout_secs": 1}),
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content.starts_with("timed out after 1s"),
            "{}",
            out.content
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
