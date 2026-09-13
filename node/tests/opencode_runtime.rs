//! Gate C, row 3: the runtime properties an OpenCode session depends on that
//! are properties of the *runner and the image*, not of OpenCode.
//!
//! Three of them, each read from `config-state.md` and each provable only
//! against a real container:
//!
//! 1. **Egress rejects rather than drops.** OpenCode's LSP downloads and its
//!    npm installs pass no `AbortSignal`, and `edit`/`write` await
//!    `lsp.touchFile` inline (§6.5), so on a blackholed network the first edit
//!    of a file blocks with nothing to break the block. On a network that
//!    refuses, the same call fails in milliseconds and the server is skipped.
//!    The first two tests measure that, by address and by name; the third runs
//!    the real binary's diagnostics path — the same `touchFile` the `edit`
//!    tool awaits — with LSP downloads *enabled*, so what bounds it is the
//!    boundary and nothing else.
//! 2. **A PID namespace with an init reaps what OpenCode does not.** `serve`
//!    installs no signal handlers, `Process.stop` is a bare single-pid
//!    SIGTERM with no escalation, and LSP children are spawned without
//!    `detached` (§6.7). Nothing upstream cleans them up; the container
//!    teardown has to.
//! 3. **The image carries the toolchain the node's configuration names.** A
//!    config naming a path the image does not have fails silently at the first
//!    edit, which is the failure mode this whole row exists to remove.
//!
//! Every test here needs Podman, the internal network, and an OpenCode harness
//! image; each skips with a message when one is missing, so a machine without
//! them still runs the rest of the suite.

#[path = "support/mod.rs"]
mod support;

use std::process::Command;
use std::time::{Duration, Instant};

use tracon::boundary::checks::REJECT_BOUND_SECS;
use tracon::config::Config;
use tracon::runner::podman::{PodmanRunner, RunSpec};
use tracon::runner::toolchain;
use tracon::runner::{Runner, RunnerCommand};

/// The hosts OpenCode reaches for on its own: the npm registry every plugin
/// and language-server install goes through, the catalogue, the web UI
/// fallback, and the release host its LSP downloads come from.
const UPSTREAM: &[&str] = &[
    "registry.npmjs.org",
    "models.opencode.ai",
    "app.opencode.ai",
    "github.com",
];

fn podman() -> Option<&'static str> {
    let ok = Command::new("podman")
        .args(["info", "--format", "{{.Host.Arch}}"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    ok.then_some("podman")
}

/// The internal, route-less network a session's container joins. Without it
/// there is no boundary to measure.
fn internal_network() -> Option<String> {
    let network = Config::default().boundary.network;
    let ok = Command::new("podman")
        .args(["network", "exists", &network])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(network)
}

/// An image that is an OpenCode harness: one that carries the toolchain
/// manifest this row's Containerfile writes. Named in
/// `TRACON_OPENCODE_IMAGE`, or found under the names `tracon setup` and this
/// row's own build use.
fn harness_image() -> Option<String> {
    let mut candidates: Vec<String> = std::env::var("TRACON_OPENCODE_IMAGE")
        .ok()
        .filter(|v| !v.is_empty())
        .into_iter()
        .collect();
    candidates.extend(
        [
            "localhost/tracon-harness-opencode",
            "localhost/tracon-harness-opencode-test",
            Config::default().boundary.harness_image.as_str(),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    candidates.into_iter().find(|image| {
        Command::new("podman")
            .args(["image", "exists", image])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && Command::new("podman")
                .args([
                    "run",
                    "--rm",
                    "--network=none",
                    image,
                    "test",
                    "-f",
                    toolchain::MANIFEST_PATH,
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    })
}

/// Everything the tests here need, or a printed reason they were skipped.
struct Runtime {
    image: String,
    network: String,
}

fn runtime() -> Option<Runtime> {
    support::state::isolate();
    if podman().is_none() {
        eprintln!("skipped: podman is not usable on this machine");
        return None;
    }
    let Some(network) = internal_network() else {
        eprintln!(
            "skipped: the internal network is not created on this machine. Run `tracon setup`."
        );
        return None;
    };
    let Some(image) = harness_image() else {
        eprintln!(
            "skipped: no OpenCode harness image on this machine. Run `tracon setup` with the \
             OpenCode harness selected, or build containers/harness-opencode and name it in \
             TRACON_OPENCODE_IMAGE."
        );
        return None;
    };
    Some(Runtime { image, network })
}

/// A runner rendering the same `podman run` line a session does, against the
/// image under test.
fn runner(rt: &Runtime) -> PodmanRunner {
    let mut cfg = Config::default();
    cfg.harness.id = "opencode".into();
    cfg.boundary.harness_image = rt.image.clone();
    cfg.boundary.network = rt.network.clone();
    PodmanRunner::new(RunSpec::from_config(&cfg, false))
}

/// Run a shell script inside the boundary and return its stdout.
async fn inside(rt: &Runtime, name: &str, script: &str) -> String {
    let out = runner(rt)
        .run_capture(RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            name: name.into(),
            ..Default::default()
        })
        .await
        .expect("the probe container ran");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A denied *address* must fail at once. There is no route out of the
/// internal network, so `connect(2)` returns rather than waiting — which is
/// exactly the property OpenCode's timeout-free downloads depend on.
#[tokio::test]
async fn a_denied_address_is_refused_rather_than_dropped() {
    let Some(rt) = runtime() else { return };
    let out = inside(
        &rt,
        "tracon-test-reject-addr",
        "s=$(date +%s); \
         curl -s -o /dev/null -m 30 --noproxy '*' http://1.1.1.1/ && echo REACHED; \
         echo \"took=$(( $(date +%s) - s ))\"",
    )
    .await;
    assert!(
        !out.contains("REACHED"),
        "the harness reached an address outside the boundary: {out}"
    );
    let took: u64 = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("took=")?.parse().ok())
        .expect("the probe reported how long the denial took");
    assert!(
        took <= REJECT_BOUND_SECS,
        "a denied address took {took}s to fail; a boundary that drops rather than rejects \
         hangs the first edit of a file, because OpenCode's download path has no timeout"
    );
}

/// And a denied *name*: resolution is a second, independent way to hang, and
/// the hosts below are the ones OpenCode reaches for without being asked.
#[tokio::test]
async fn every_upstream_the_harness_reaches_for_fails_fast() {
    let Some(rt) = runtime() else { return };
    let script = UPSTREAM
        .iter()
        .map(|host| {
            format!(
                "s=$(date +%s); \
                 curl -s -o /dev/null -m 30 --noproxy '*' https://{host}/ && echo REACHED={host}; \
                 echo \"took={host}=$(( $(date +%s) - s ))\";"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let out = inside(&rt, "tracon-test-reject-names", &script).await;
    for host in UPSTREAM {
        assert!(
            !out.contains(&format!("REACHED={host}")),
            "the harness reached {host} directly: {out}"
        );
        let took: u64 = out
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix(&format!("took={host}="))?
                    .parse()
                    .ok()
            })
            .unwrap_or_else(|| panic!("the probe reported no timing for {host}: {out}"));
        assert!(
            took <= REJECT_BOUND_SECS,
            "{host} took {took}s to fail, over the {REJECT_BOUND_SECS}s bound"
        );
    }
}

/// The real binary, on the path the `edit` tool awaits inline.
///
/// `opencode debug lsp diagnostics <file>` calls `LSP.touchFile(file, "full")`
/// — the same serial, awaited spawn loop `edit.ts:197` and `write.ts:75` block
/// on. This runs it with `"lsp": true` and **without**
/// `OPENCODE_DISABLE_LSP_DOWNLOAD`: the belt is deliberately off, so the only
/// thing that can bound the call is the boundary refusing. A `.sh` file is the
/// probe because the `bash` server's root detector is the directory itself and
/// its install path is a plain registry fetch — no project file has to exist
/// for the download to be attempted.
#[tokio::test]
async fn the_first_diagnostics_of_a_file_with_no_server_completes_rather_than_hanging() {
    let Some(rt) = runtime() else { return };
    // npm's own retry schedule (two retries, 10s then 60s) would dominate the
    // measurement and say nothing about the network, so it is turned off: what
    // is under test is how long the boundary takes to refuse, not how patient
    // npm is about being refused.
    let script = "set -e; \
         mkdir -p /tmp/h /tmp/probe; cd /tmp/probe; echo 'echo hi' > a.sh; \
         printf '%s' '{\"lsp\": true}' > /tmp/oc.json; \
         s=$(date +%s); \
         timeout 240 opencode debug lsp diagnostics /tmp/probe/a.sh > /tmp/out 2>&1; \
         echo \"rc=$? took=$(( $(date +%s) - s ))\"";
    let out = runner(&rt)
        .run_capture(RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            name: "tracon-test-lsp-touch".into(),
            env: vec![
                ("HOME".into(), "/tmp/h".into()),
                ("XDG_CONFIG_HOME".into(), "/tmp/h/config".into()),
                ("XDG_DATA_HOME".into(), "/tmp/h/data".into()),
                ("XDG_CACHE_HOME".into(), "/tmp/h/cache".into()),
                ("XDG_STATE_HOME".into(), "/tmp/h/state".into()),
                ("OPENCODE_CONFIG".into(), "/tmp/oc.json".into()),
                ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "true".into()),
                ("OPENCODE_DISABLE_MODELS_FETCH".into(), "true".into()),
                ("npm_config_fetch_retries".into(), "0".into()),
            ],
            ..Default::default()
        })
        .await
        .expect("the probe container ran");
    let out = String::from_utf8_lossy(&out.stdout).into_owned();
    let (rc, took) = out
        .lines()
        .find_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("rc=")?;
            let (rc, took) = rest.split_once(" took=")?;
            Some((rc.parse::<i32>().ok()?, took.parse::<u64>().ok()?))
        })
        .unwrap_or_else(|| panic!("the probe did not report a result: {out}"));
    assert_ne!(
        rc, 124,
        "the first diagnostics call never returned: this boundary drops rather than rejects, \
         and every edit of a file whose server is missing will block on it"
    );
    // Generous next to the 50 ms a refusal actually takes, and still an order
    // of magnitude under the five-minute window a blackholed registry opens
    // (`config-state.md` §4.3).
    assert!(
        took <= 60,
        "the first diagnostics call took {took}s; it completes in about a second when the \
         boundary refuses"
    );
}

/// Nothing the harness started outlives the container.
///
/// OpenCode reaps none of it: `serve` installs no signal handler, so a stop
/// runs no finalizer at all, and even a clean exit calls `process.exit()` in a
/// `finally` before pending ones run. The container's PID namespace is what
/// makes that safe, and `--init` is what keeps a double-forked formatter from
/// being reparented somewhere the namespace teardown would still have to
/// collect. This starts two long sleeps — one a child of the container's
/// command, one backgrounded away from it, which is the shape a formatter
/// spawned `detached` has — and asserts both are gone.
#[tokio::test]
async fn nothing_the_harness_started_survives_the_runner_killing_it() {
    let Some(rt) = runtime() else { return };
    // Two improbable durations, so what is looked for on the host afterwards
    // can only be these processes.
    let (child, orphan) = ("604811", "604812");
    let name = format!("tracon-test-reap-{}", std::process::id());
    let runner = runner(&rt);
    let spawned = runner
        .spawn(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("sleep {orphan} & exec sleep {child}"),
            ],
            name: name.clone(),
            ..Default::default()
        })
        .await
        .expect("the container started");

    let top = |want: &str| {
        let out = Command::new("podman")
            .args(["top", &name, "args"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        out.contains(want)
    };
    let mut started = false;
    for _ in 0..200 {
        if top(child) && top(orphan) {
            started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(started, "the container never started both processes");
    // An init, not the harness, at the head of the namespace.
    let pid_one = Command::new("podman")
        .args(["exec", &name, "cat", "/proc/1/cmdline"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).replace('\0', " "))
        .unwrap_or_default();
    assert!(
        pid_one.contains("init") || pid_one.contains("catatonit"),
        "PID 1 of the harness namespace is {pid_one:?}, not an init"
    );

    let killed = Instant::now();
    runner.kill(&name).await.expect("the runner killed it");
    drop(spawned);

    let gone = Command::new("podman")
        .args(["container", "exists", &name])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);
    assert!(gone, "the container is still there after kill");
    // And nothing of it is left on this host: a container process that
    // survived its container would show up here under its own improbable
    // argument.
    for marker in [child, orphan] {
        let host = Command::new("ps")
            .args(["-eo", "args"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        assert!(
            !host.contains(&format!("sleep {marker}")),
            "a process the harness started is still running {:?} after the runner killed it",
            marker
        );
    }
    assert!(
        killed.elapsed()
            < Duration::from_secs(u64::from(Config::default().boundary.stop_timeout_secs) + 20),
        "the kill took longer than the stop timeout allows"
    );
}

/// The configuration the node writes is one the pinned binary accepts, and the
/// image has every binary it names. Both halves matter: an `lsp` entry the
/// schema rejects is dropped with a diagnostic nobody reads, and a `command`
/// pointing at nothing is skipped in silence at the first edit.
#[tokio::test]
async fn the_image_has_every_binary_the_rendered_configuration_names() {
    let Some(rt) = runtime() else { return };
    let status = toolchain::probe(&runner(&rt)).await;
    assert_eq!(
        status.image_revision.as_deref(),
        Some(status.revision.as_str()),
        "the image was built from another revision of the toolchain profile than this binary's"
    );
    for tool in status.tools.iter().filter(|t| !t.path.is_empty()) {
        assert_eq!(
            tool.state,
            toolchain::ToolState::Configured,
            "the image has no {} at {}",
            tool.id,
            tool.path
        );
    }
    assert!(status.complete());

    // And the binary reads the rendering back unchanged, which is the only
    // evidence that its schema accepted it.
    let (lsp, formatter) = toolchain::lsp_formatter_config();
    let document = serde_json::json!({ "lsp": lsp, "formatter": formatter });
    let script = format!(
        "set -e; mkdir -p /tmp/h; \
         cat > /tmp/oc.json <<'TRACON_EOF'\n{document}\nTRACON_EOF\n\
         opencode serve --pure --hostname 127.0.0.1 --port 4399 >/tmp/serve.log 2>&1 & \
         for _ in $(seq 1 100); do \
           curl -sf -m 2 --noproxy '*' http://127.0.0.1:4399/global/health >/dev/null && break; \
           sleep 0.2; \
         done; \
         curl -sf -m 10 --noproxy '*' http://127.0.0.1:4399/config"
    );
    let out = runner(&rt)
        .run_capture(RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), script],
            name: "tracon-test-config-shape".into(),
            env: vec![
                ("HOME".into(), "/tmp/h".into()),
                ("XDG_CONFIG_HOME".into(), "/tmp/h/config".into()),
                ("XDG_DATA_HOME".into(), "/tmp/h/data".into()),
                ("XDG_CACHE_HOME".into(), "/tmp/h/cache".into()),
                ("XDG_STATE_HOME".into(), "/tmp/h/state".into()),
                ("OPENCODE_CONFIG".into(), "/tmp/oc.json".into()),
                ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "true".into()),
                ("OPENCODE_DISABLE_MODELS_FETCH".into(), "true".into()),
                ("OPENCODE_DISABLE_LSP_DOWNLOAD".into(), "true".into()),
            ],
            ..Default::default()
        })
        .await
        .expect("the probe container ran");
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    let served: serde_json::Value =
        serde_json::from_str(body.trim()).unwrap_or_else(|e| panic!("GET /config: {e}: {body}"));
    let profile = toolchain::profile();
    for tool in &profile.lsp {
        assert_eq!(
            served["lsp"][&tool.id]["command"],
            serde_json::json!(tool.command),
            "the binary did not read back the command for {}",
            tool.id
        );
    }
    assert_eq!(
        served["lsp"]["typescript"]["initialization"]["tsserver"]["path"],
        serde_json::json!(profile
            .lsp
            .iter()
            .find(|t| t.id == "typescript")
            .and_then(|t| t.initialization.as_ref())
            .map(|i| i["tsserver"]["path"].clone())
            .unwrap())
    );
    for tool in &profile.formatter {
        assert_eq!(
            served["formatter"][&tool.id]["command"],
            serde_json::json!(tool.command),
            "the binary did not read back the command for {}",
            tool.id
        );
    }
    // The three that install themselves are off or overridden, and nothing in
    // the rendering leaves one of them to `Npm.which`.
    assert_eq!(
        served["formatter"]["oxfmt"]["disabled"],
        serde_json::json!(true)
    );
    assert_eq!(
        served["formatter"]["biome"]["disabled"],
        serde_json::json!(true)
    );
}
