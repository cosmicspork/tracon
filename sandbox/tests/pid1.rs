//! `tracon-sandbox` as a container's PID 1: it reaps orphans, and a SIGTERM
//! to the container becomes an orderly teardown its services see.
//!
//! Needs a container runtime and a static build of the binary, so it runs
//! only when asked to:
//!
//! ```sh
//! TRACON_SANDBOX_CONTAINER_RUNTIME=podman \
//!   cargo test --release --target x86_64-unknown-linux-musl -p tracon-sandbox --test pid1
//! ```
//!
//! `TRACON_SANDBOX_STATIC_BIN` names the binary when it is not this build's,
//! and `TRACON_SANDBOX_TEST_IMAGE` the image it runs in (anything with a
//! POSIX shell and `/proc`). The node's socket is created the way a Podman
//! node on an SELinux host must create it: labelled for `container_t`, in a
//! directory labelled `container_file_t`, with the container's own labels
//! left on.

mod support;

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracon_sandbox::carrier::{bind_unix_labelled, label_path, Listener};
use tracon_sandbox::node::{SandboxServer, ServerOptions};
use tracon_sandbox::protocol::{Event, ServiceSpec, ServiceState, SpawnSpec};
use tracon_sandbox::tls::NodeIdentity;

const DEFAULT_IMAGE: &str = "docker.io/library/busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";
const SOCKET_CONTEXT: &str = "system_u:system_r:container_t:s0";
const DIR_CONTEXT: &str = "system_u:object_r:container_file_t:s0";

/// Removes the container however the test ends.
struct Container {
    runtime: String,
    cidfile: std::path::PathBuf,
}

impl Drop for Container {
    fn drop(&mut self) {
        if let Ok(id) = std::fs::read_to_string(&self.cidfile) {
            let _ = std::process::Command::new(&self.runtime)
                .args(["rm", "-f", id.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn proc_state(stat: &str) -> Option<(i32, char, i32)> {
    // `pid (comm) state ppid ...`; comm may itself hold spaces and brackets.
    let (head, tail) = stat.rsplit_once(')')?;
    let pid = head.split_once(' ')?.0.trim().parse().ok()?;
    let mut fields = tail.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((pid, state, ppid))
}

#[tokio::test]
async fn runs_as_pid_1_in_a_container() {
    let Ok(runtime) = std::env::var("TRACON_SANDBOX_CONTAINER_RUNTIME") else {
        eprintln!("skipped: set TRACON_SANDBOX_CONTAINER_RUNTIME to docker or podman");
        return;
    };
    let binary = std::env::var("TRACON_SANDBOX_STATIC_BIN").unwrap_or_else(|_| support::BIN.into());
    let image = std::env::var("TRACON_SANDBOX_TEST_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.into());

    let shared = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    // Readable by whatever uid the container's root maps to.
    std::fs::set_permissions(
        shared.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    label_path(shared.path(), DIR_CONTEXT).unwrap();
    std::fs::copy(&binary, shared.path().join("tracon-sandbox")).unwrap();
    let identity = NodeIdentity::generate().unwrap();
    let server = SandboxServer::new(&identity, ServerOptions::default()).unwrap();
    let token = server.tokens().issue("pid1");
    let socket = shared.path().join("node.sock");
    let listener = Listener::Unix(bind_unix_labelled(&socket, Some(SOCKET_CONTEXT)).unwrap());
    std::fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();

    let cidfile = scratch.path().join("cid");
    let _container = Container {
        runtime: runtime.clone(),
        cidfile: cidfile.clone(),
    };
    let mut run = Command::new(&runtime);
    run.args(["run", "--rm", "--network", "none", "--cidfile"])
        .arg(&cidfile)
        .arg("-v")
        .arg(format!("{}:/run/tracon", shared.path().display()))
        .args(["-e", "TRACON_SANDBOX_SOCKET=/run/tracon/node.sock"])
        .args(["-e", &format!("TRACON_SANDBOX_PIN={}", identity.pin())])
        .args(["-e", &format!("TRACON_SANDBOX_TOKEN={token}")])
        .args(["--entrypoint", "/run/tracon/tracon-sandbox"])
        .arg(&image)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let mut container = run.spawn().unwrap();

    // The first run may pull the image.
    let conn = tokio::time::timeout(Duration::from_secs(120), server.accept_from(&listener))
        .await
        .expect("the sandbox dialled in time")
        .unwrap();
    assert_eq!(conn.info().pid, 1);
    assert!(conn.info().init);

    // An orphan: `sh` exits at once and leaves `sleep` behind, which the
    // kernel hands to PID 1. Its exit must be reaped, not left a zombie.
    let output = conn
        .spawn(SpawnSpec::new([
            "sh",
            "-c",
            "sleep 2 >/dev/null 2>&1 & echo $!",
        ]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    let orphan: i32 = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let stat = conn
        .spawn(SpawnSpec::new(["cat", &format!("/proc/{orphan}/stat")]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    let (_, _, ppid) = proc_state(&String::from_utf8_lossy(&stat.stdout)).expect("orphan alive");
    assert_eq!(ppid, 1, "the orphan was adopted by the sandbox");
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let all = conn
        .spawn(SpawnSpec::new([
            "sh",
            "-c",
            "cat /proc/[0-9]*/stat 2>/dev/null",
        ]))
        .await
        .unwrap()
        .output()
        .await
        .unwrap();
    let states: Vec<_> = String::from_utf8_lossy(&all.stdout)
        .lines()
        .filter_map(proc_state)
        .collect();
    assert!(!states.is_empty());
    assert!(
        states
            .iter()
            .all(|(pid, state, _)| *state != 'Z' && *pid != orphan),
        "zombies or the orphan remain: {states:?}"
    );

    // SIGTERM to the container is SIGTERM to PID 1: the service hears its
    // stop signal and the sandbox exits cleanly, well before a kill.
    let mut events = conn.events().await.unwrap();
    conn.services()
        .start(vec![ServiceSpec::new(
            "trap",
            [
                "sh",
                "-c",
                "trap 'echo got-term; exit 0' TERM; while :; do sleep 0.1; done",
            ],
        )])
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(
            event,
            Event::Service {
                state: ServiceState::Ready,
                ..
            }
        ) {
            break;
        }
    }
    let cid = std::fs::read_to_string(&cidfile).unwrap();
    let killed = Command::new(&runtime)
        .args(["kill", "--signal", "TERM", cid.trim()])
        .status()
        .await
        .unwrap();
    assert!(killed.success());
    let mut heard = false;
    let mut stopped = false;
    while let Ok(Ok(Some(event))) =
        tokio::time::timeout(Duration::from_secs(10), events.next()).await
    {
        match event {
            Event::Log { line, .. } if line == "got-term" => heard = true,
            Event::Service {
                state: ServiceState::Stopped,
                ..
            } => stopped = true,
            _ => {}
        }
    }
    assert!(heard && stopped, "heard {heard}, stopped {stopped}");
    let status = tokio::time::timeout(Duration::from_secs(10), container.wait())
        .await
        .expect("the container exited before any kill")
        .unwrap();
    assert!(status.success(), "{status:?}");
}
