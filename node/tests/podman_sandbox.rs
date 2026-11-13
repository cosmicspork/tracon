//! The Podman sandbox provider and the protocol runner against real Podman:
//! commands run in a container whose only process the node started is
//! `tracon-sandbox`, with everything else over the sandbox protocol. A
//! harness session's turns through the node's gateways are
//! `podman_tunnels`' and `podman_sandbox_files`'.
//!
//! Runs only when asked for:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 TRACON_SANDBOX_BUNDLE=<bundle>/<version>/amd64 \
//!   cargo test -p tracon --test podman_sandbox -- --ignored --test-threads 1
//! ```
//!
//! The bundle needs `bin/tracon-sandbox` built from this tree
//! (`scripts/harness-bundle.sh --sandbox`). The image is
//! `TRACON_SANDBOX_TEST_IMAGE` (default this version's
//! `tracon-sandbox-base`, which must be in the local store). Everything the tests make is named
//! with `TRACON_SANDBOX_TEST_PREFIX` (default `tracon-test-sbx-`) and removed
//! again; nothing of a node's is used.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracon::config::{Config, HarnessListen};
use tracon::runner::podman::RunSpec;
use tracon::runner::{Mount, Runner, RunnerCommand};

use support::podman::{podman, requested};

struct Rig {
    prefix: String,
    cfg: Config,
    image: String,
    volumes: Vec<String>,
    _short: tempfile::TempDir,
}

impl Rig {
    /// A configuration running Podman's sandboxes from the bundle on the
    /// test's image, and somewhere short for the sockets.
    fn new(tag: &str) -> Option<Rig> {
        if !requested() {
            return None;
        }
        support::state::isolate();
        let bundle = std::env::var("TRACON_SANDBOX_BUNDLE")
            .map(PathBuf::from)
            .expect("TRACON_PODMAN_TESTS=1 needs TRACON_SANDBOX_BUNDLE");
        assert!(bundle.join("bin/tracon-sandbox").is_file());
        let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE").unwrap_or_else(|_| {
            format!(
                "ghcr.io/cosmicspork/tracon-sandbox-base:{}",
                env!("CARGO_PKG_VERSION")
            )
        });
        assert!(podman(&["image", "exists", &image]).is_some(), "{image}");
        let prefix = format!(
            "{}{tag}-",
            std::env::var("TRACON_SANDBOX_TEST_PREFIX")
                .unwrap_or_else(|_| "tracon-test-sbx-".into())
        );
        let short = tempfile::Builder::new()
            .prefix("sbx")
            .tempdir_in("/tmp")
            .unwrap();
        let mut cfg = Config::default();
        cfg.boundary.gateway_container = format!("{prefix}gw");
        cfg.sandbox.base_image = Some(image.clone());
        cfg.gateway.harness_listen = HarnessListen::Unix(short.path().join("harness.sock"));
        cfg.sandbox.bundle = Some(bundle);
        Some(Rig {
            prefix,
            cfg,
            image,
            volumes: Vec::new(),
            _short: short,
        })
    }

    fn name(&self, what: &str) -> String {
        format!("{}{what}", self.prefix)
    }

    fn volume(&mut self, what: &str) -> String {
        let name = self.name(what);
        let _ = podman(&["volume", "rm", "-f", &name]);
        self.volumes.push(name.clone());
        name
    }

    fn runner(&self, mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = RunSpec::from_config(&self.cfg);
        spec.image = self.image.clone();
        spec.extra_mounts = mounts;
        tracon::boundary::podman::protocol_runner(&self.cfg, spec)
    }

    fn command(&self, what: &str, script: &str) -> RunnerCommand {
        RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            name: self.name(what),
            ..Default::default()
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(ids) = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.prefix)]) {
            for id in ids.lines() {
                let _ = podman(&["rm", "-f", id]);
            }
        }
        for volume in &self.volumes {
            let _ = podman(&["volume", "rm", "-f", volume]);
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_command_runs_confined_with_the_bundle_read_only_and_the_supervisor_as_pid_1() {
    let Some(mut rig) = Rig::new("cmd") else {
        return;
    };
    let work = rig.volume("work");
    let runner = rig.runner(Vec::new());
    let mut cmd = rig.command(
        "probe",
        "tr '\\0' ' ' </proc/1/cmdline; echo; cat /proc/self/attr/current; echo; \
         id -u; pwd; echo \"$GREETING $LENT\"; \
         if touch /opt/tracon/x 2>/dev/null; then echo bundle-writable; else echo bundle-read-only; fi; \
         if ls /run/podman-init >/dev/null 2>&1; then echo has-init; fi; \
         echo kept > /work/kept; echo to-stderr >&2; exit 5",
    );
    cmd.env = vec![("GREETING".into(), "hello".into())];
    cmd.secret_env = tracon::runner::SecretEnv::new(vec![("LENT".into(), "lent-value".into())]);
    cmd.mounts = vec![Mount::volume(&work, "/work", false)];
    let out = runner.run_capture(cmd).await.unwrap();
    let stdout = text(&out.stdout);
    eprintln!("{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].starts_with("/opt/tracon/bin/tracon-sandbox"),
        "{stdout}"
    );
    assert!(lines[1].contains(":container_t:"), "confined: {stdout}");
    // The stock base's own user, mapped to the node's.
    assert_eq!(lines[2], "65532");
    assert_eq!(lines[3], "/work");
    assert_eq!(lines[4], "hello lent-value");
    assert_eq!(lines[5], "bundle-read-only");
    assert_eq!(text(&out.stderr), "to-stderr\n");
    assert_eq!(out.status.code(), Some(5));

    // The sandbox is gone, and with it its socket directory; the volume
    // kept what was written.
    let name = runner.capture_name(&rig.name("probe"));
    assert!(podman(&["container", "exists", &name]).is_none());
    let sockets = rig.cfg.sandbox_socket_dir();
    assert_eq!(std::fs::read_dir(&sockets).unwrap().count(), 0);
    let mut again = rig.command("again", "cat /work/kept");
    again.mounts = vec![Mount::volume(&work, "/work", true)];
    assert_eq!(
        text(&runner.run_capture(again).await.unwrap().stdout),
        "kept\n"
    );
}

#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn under_init_the_supervisor_is_a_child_and_still_runs_the_command() {
    let Some(mut rig) = Rig::new("init") else {
        return;
    };
    rig.cfg.runtime.podman.sandbox_init = true;
    let out = rig
        .runner(Vec::new())
        .run_capture(rig.command(
            "init",
            "tr '\\0' ' ' </proc/1/cmdline; echo; ps -o pid=,args= -p $PPID 2>/dev/null || cat /proc/$PPID/cmdline",
        ))
        .await
        .unwrap();
    let stdout = text(&out.stdout);
    eprintln!("{stdout}");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let mut lines = stdout.lines();
    assert!(
        lines.next().unwrap().starts_with("/run/podman-init "),
        "{stdout}"
    );
    // The command's parent, the supervisor, is not PID 1.
    let parent = lines.next().unwrap().trim();
    assert!(parent.contains("tracon-sandbox"), "{stdout}");
    assert!(!parent.starts_with("1 "), "{stdout}");
}

#[tokio::test]
#[ignore = "needs real Podman; run with TRACON_PODMAN_TESTS=1"]
async fn a_harness_streams_stdio_with_backpressure_and_ends_when_killed() {
    let Some(rig) = Rig::new("stdio") else {
        return;
    };
    let runner = rig.runner(Vec::new());
    let spawned = runner.spawn(rig.command("cat", "exec cat")).await.unwrap();
    let mut stdin = spawned.stdin;
    let mut stdout = spawned.stdout;
    // Far more than any window: the writer can only finish if the reader
    // keeps taking.
    let sent: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let expected = sent.clone();
    let writer = tokio::spawn(async move {
        stdin.write_all(&sent).await.unwrap();
        stdin.shutdown().await.unwrap();
    });
    let mut got = Vec::new();
    stdout.read_to_end(&mut got).await.unwrap();
    writer.await.unwrap();
    assert_eq!(got.len(), expected.len());
    assert!(got == expected, "the bytes came back changed");
    assert_eq!(spawned.done.await.unwrap(), 0);

    let long = runner
        .spawn(rig.command("sleeper", "exec sleep 600"))
        .await
        .unwrap();
    let name = rig.name("sleeper");
    assert!(podman(&["container", "exists", &name]).is_some());
    // A runner made afresh, as the session's kill path makes one.
    rig.runner(Vec::new()).kill(&name).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(30), long.done)
        .await
        .expect("the harness outlived its sandbox");
    assert!(ended.is_err() || ended.unwrap() != 0);
    assert!(podman(&["container", "exists", &name]).is_none());
}
