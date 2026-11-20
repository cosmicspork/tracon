//! The plugin supervisor against a fake plugin (`tests/support/fake_plugin.rs`):
//! it starts on `ensure` with only the three contract variables, its socket and
//! data directories private; a crash restarts it with backoff and repeated ones
//! leave it unhealthy; it stops when idle, when the plugin is removed, and when
//! the node shuts down; a use that meets a stop starts it again; and a node
//! restart waits for it while it runs.

#[path = "support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use tracon::{
    config::Config,
    mcp::upstream,
    plugin::{
        install,
        supervisor::{State, Supervisor, Timings},
    },
    session::Manager,
    store::Store,
    stream::Bus,
};

const PLUGIN: &str = "fake";
const NODE_KEY: &str = "ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12";

/// Install the fake plugin into `state` as the operator's allowed card would.
fn install_fake(store: &Store, state: &Path) {
    support::plugin::install(
        store,
        state,
        &format!(
            "name = \"{PLUGIN}\"\nversion = \"1.0.0\"\ncontract = 1\nexec = [\"bin/{PLUGIN}\"]\n\n[[tool]]\nname = \"query\"\nhint = \"read\"\n"
        ),
    );
}

struct Rig {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    store: Arc<Store>,
    sup: Arc<Supervisor>,
    idle_ms: Arc<AtomicU64>,
}

fn rig() -> Rig {
    support::state::isolate();
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().to_path_buf();
    let store = Arc::new(Store::open_in_memory().unwrap());
    install_fake(&store, &state);
    let idle_ms = Arc::new(AtomicU64::new(60_000));
    let idle = idle_ms.clone();
    let sup = Arc::new(Supervisor::with_timings(
        store.clone(),
        state.clone(),
        NODE_KEY.into(),
        Arc::new(move |_: &str| Duration::from_millis(idle.load(Ordering::SeqCst))),
        Timings {
            ready_within: Duration::from_secs(2),
            backoff_initial: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
            reset_after: Duration::from_secs(600),
            stop_grace: Duration::from_millis(500),
        },
    ));
    Rig {
        _tmp: tmp,
        state,
        store,
        sup,
        idle_ms,
    }
}

impl Rig {
    fn data(&self, channel: &str) -> PathBuf {
        install::data_dir(&self.state, PLUGIN).join(channel)
    }

    /// Set what the fake does at its next start.
    fn mode(&self, channel: &str, mode: &str) {
        std::fs::create_dir_all(self.data(channel)).unwrap();
        std::fs::write(self.data(channel).join("mode"), mode).unwrap();
    }

    fn starts(&self, channel: &str) -> Vec<u128> {
        std::fs::read_to_string(self.data(channel).join("started"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.split(' ').next().unwrap().parse().unwrap())
            .collect()
    }

    fn pid(&self, channel: &str) -> i32 {
        std::fs::read_to_string(self.data(channel).join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn status(&self, channel: &str) -> tracon::plugin::supervisor::InstanceStatus {
        self.sup
            .status()
            .into_iter()
            .find(|i| i.plugin == PLUGIN && i.channel == channel)
            .expect("an instance")
    }

    async fn until(&self, what: &str, within: Duration, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + within;
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "{what}: not within {within:?}; {:?}",
                self.sup.status()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn gone(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn ensure_starts_it_and_it_answers_on_its_socket() {
    let r = rig();
    let started = Instant::now();
    let endpoint = r.sup.ensure(PLUGIN, "personal").await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(endpoint.socket.exists());
    let http = upstream::unix_client(&endpoint.socket);
    upstream::initialize_unix(&http, Duration::from_secs(2))
        .await
        .unwrap();
    let status = r.status("personal");
    assert_eq!(status.state, State::Ready);
    assert_eq!(status.pid, Some(r.pid("personal") as u32));
    assert_eq!(status.version.as_deref(), Some("1.0.0"));
    assert_eq!(r.sup.running(), [format!("{PLUGIN}/personal")]);
    // Asked again, the same process answers.
    let again = r.sup.ensure(PLUGIN, "personal").await.unwrap();
    assert_eq!(again.socket, endpoint.socket);
    assert_eq!(r.starts("personal").len(), 1);
    r.sup.shutdown().await;
}

#[tokio::test]
async fn its_environment_holds_only_the_three_contract_variables() {
    let r = rig();
    let first = r.sup.ensure(PLUGIN, "personal").await.unwrap();
    let env: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(r.data("personal").join("env.json")).unwrap(),
    )
    .unwrap();
    let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        ["TRACON_CONTRACT", "TRACON_NODE_KEY", "TRACON_PLUGIN_TOKEN"]
    );
    let token = env["TRACON_PLUGIN_TOKEN"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    assert!(token
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(&*first.token, token);
    assert_eq!(env["TRACON_NODE_KEY"], NODE_KEY);
    assert_eq!(env["TRACON_CONTRACT"], "1");

    // Each start has its own token.
    let second = r.sup.restart(PLUGIN, "personal").await.unwrap();
    assert_ne!(second.token, first.token);
    let env: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(r.data("personal").join("env.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(env["TRACON_PLUGIN_TOKEN"], &*second.token);
    r.sup.shutdown().await;
}

#[tokio::test]
async fn its_socket_and_data_directories_are_private() {
    let r = rig();
    let endpoint = r.sup.ensure(PLUGIN, "personal").await.unwrap();
    assert_eq!(mode_of(endpoint.socket.parent().unwrap()), 0o700);
    let data = r.data("personal");
    assert_eq!(mode_of(&data), 0o700);
    assert_eq!(mode_of(data.parent().unwrap()), 0o700);
    assert_eq!(mode_of(&install::root(&r.state).join("data")), 0o700);
    let cwd = std::fs::read_to_string(data.join("cwd")).unwrap();
    assert_eq!(
        std::fs::canonicalize(cwd).unwrap(),
        std::fs::canonicalize(&data).unwrap()
    );
    r.sup.shutdown().await;
}

#[tokio::test]
async fn a_crash_restarts_it_after_a_backoff() {
    let r = rig();
    r.mode("personal", "crash");
    r.sup.ensure(PLUGIN, "personal").await.unwrap();
    r.until("a restart", Duration::from_secs(5), |r| {
        r.status("personal").restarts >= 1
    })
    .await;
    assert!(r
        .status("personal")
        .last_error
        .unwrap()
        .contains("exited with code 1"));
    r.mode("personal", "serve");
    r.until("ready again", Duration::from_secs(5), |r| {
        let s = r.status("personal");
        s.state == State::Ready && r.starts("personal").len() == s.restarts as usize + 1
    })
    .await;
    // Ready after this one stays up past the crash's 200 ms.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(r.status("personal").state, State::Ready);
    r.sup.shutdown().await;
}

#[tokio::test]
async fn repeated_failures_leave_it_unhealthy_until_it_is_asked_for_again() {
    let r = rig();
    r.mode("personal", "exit");
    let error = r.sup.ensure(PLUGIN, "personal").await.unwrap_err();
    assert!(error.contains("exited with code 3"), "{error}");
    let status = r.status("personal");
    assert_eq!(status.state, State::Unhealthy);
    assert_eq!(status.restarts, 3);
    // Started at once, then after 50, 100 and 200 ms.
    let starts = r.starts("personal");
    assert_eq!(starts.len(), 4, "{starts:?}");
    for (gap, wait) in starts.windows(2).zip([50, 100, 200]) {
        assert!(gap[1] - gap[0] >= wait, "{starts:?}");
    }
    assert!(r.sup.running().is_empty());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(r.starts("personal").len(), 4, "not started again");

    r.mode("personal", "serve");
    r.sup.ensure(PLUGIN, "personal").await.unwrap();
    let status = r.status("personal");
    assert_eq!(status.state, State::Ready);
    assert_eq!(status.last_error, None);
    r.sup.shutdown().await;
}

#[tokio::test]
async fn one_not_ready_in_time_is_killed_and_counts_as_a_failure() {
    let r = rig();
    r.mode("personal", "hang");
    let sup = r.sup.clone();
    let ensure = tokio::spawn(async move { sup.ensure(PLUGIN, "personal").await });
    // The spawned `ensure` may not have made the instance yet.
    r.until("the first failure", Duration::from_secs(5), |r| {
        r.sup.status().iter().any(|i| i.restarts >= 1)
    })
    .await;
    let error = r.status("personal").last_error.unwrap();
    assert!(error.contains("not ready within"), "{error}");
    let first = r.starts("personal");
    assert!(!first.is_empty());
    // The first one's pid file may already be the second's; the halted one
    // is gone either way once the instance is stopped. Shut down rather than
    // stopped, which the waiting `ensure` would answer by starting it again.
    r.sup.shutdown().await;
    assert!(gone(r.pid("personal")));
    assert!(ensure.await.unwrap().is_err());
}

#[tokio::test]
async fn it_stops_once_idle_and_a_use_puts_that_off() {
    let r = rig();
    r.idle_ms.store(400, Ordering::SeqCst);
    let endpoint = r.sup.ensure(PLUGIN, "personal").await.unwrap();
    let pid = r.pid("personal");
    // Used every 100 ms, it stays.
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        r.sup.ensure(PLUGIN, "personal").await.unwrap();
    }
    assert_eq!(r.status("personal").state, State::Ready);
    assert_eq!(
        r.pid("personal"),
        pid,
        "{:?}",
        r.sup.logs(PLUGIN, "personal", 50)
    );
    r.until("an idle stop", Duration::from_secs(3), |r| {
        r.status("personal").state == State::Stopped
    })
    .await;
    assert!(gone(pid));
    assert!(!endpoint.socket.exists());
    assert!(r.sup.running().is_empty());
    let logs = r.sup.logs(PLUGIN, "personal", 50);
    assert!(logs.iter().any(|l| l.contains("idle")), "{logs:?}");
}

#[tokio::test]
async fn removing_the_plugin_stops_it() {
    let r = rig();
    r.sup.ensure(PLUGIN, "personal").await.unwrap();
    let pid = r.pid("personal");
    tracon::plugin::remove(&r.store, &r.state, PLUGIN, false).unwrap();
    r.sup.stop_plugin(PLUGIN).await;
    assert_eq!(r.status("personal").state, State::Stopped);
    assert!(gone(pid));
    let error = r.sup.ensure(PLUGIN, "personal").await.unwrap_err();
    assert!(error.contains("not installed"), "{error}");
    assert_eq!(r.status("personal").state, State::Unhealthy);
}

#[tokio::test]
async fn a_use_that_meets_a_stop_starts_it_again() {
    let r = rig();
    r.mode("personal", "hang");
    let sup = r.sup.clone();
    let ensure = tokio::spawn(async move { sup.ensure(PLUGIN, "personal").await });
    // It has read its mode once it has noted its start.
    r.until("the first start", Duration::from_secs(5), |r| {
        !r.starts("personal").is_empty()
    })
    .await;
    // Stopped as an update stops it, while the use waits; the next start
    // serves.
    r.mode("personal", "serve");
    r.sup.stop_plugin(PLUGIN).await;
    let endpoint = ensure.await.unwrap().unwrap();
    assert!(endpoint.socket.exists());
    assert_eq!(r.status("personal").state, State::Ready);
    assert_eq!(r.starts("personal").len(), 2);
    r.sup.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_every_instance_and_starts_none() {
    let r = rig();
    r.sup.ensure(PLUGIN, "personal").await.unwrap();
    r.sup.ensure(PLUGIN, "work").await.unwrap();
    let pids = [r.pid("personal"), r.pid("work")];
    assert_eq!(r.sup.running().len(), 2);
    r.sup.shutdown().await;
    assert!(r.sup.running().is_empty());
    assert!(pids.into_iter().all(gone));
    let error = r.sup.ensure(PLUGIN, "personal").await.unwrap_err();
    assert!(error.contains("shutting down"), "{error}");
}

#[tokio::test]
async fn its_output_is_kept_in_memory_and_in_its_log_file() {
    let r = rig();
    r.sup.ensure(PLUGIN, "personal").await.unwrap();
    r.until("its output", Duration::from_secs(2), |r| {
        let logs = r.sup.logs(PLUGIN, "personal", 100);
        logs.iter().any(|l| l == "fake plugin started")
            && logs.iter().any(|l| l == "fake plugin says hello on stderr")
    })
    .await;
    let logs = r.sup.logs(PLUGIN, "personal", 100);
    assert!(logs.iter().any(|l| l == "[tracon] ready"), "{logs:?}");
    let file = install::root(&r.state)
        .join("logs")
        .join(PLUGIN)
        .join("personal.log");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("fake plugin started"), "{text}");
    assert_eq!(mode_of(&file), 0o600);
    assert_eq!(mode_of(file.parent().unwrap()), 0o700);
    assert!(r.sup.logs(PLUGIN, "personal", 1).len() == 1);
    r.sup.shutdown().await;
}

#[tokio::test]
async fn a_node_restart_waits_for_a_running_instance() {
    support::state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let cfg = Arc::new(Config::default());
    let tools = Arc::new(tracon::mcp::Tools {
        broker: Arc::new(Default::default()),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
        upstream: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg,
        NODE_KEY.into(),
        tools,
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    install_fake(&store, &Config::state_dir());
    assert!(tracon::restart::busy(&manager).idle());
    manager.plugins().ensure(PLUGIN, "personal").await.unwrap();
    let busy = tracon::restart::busy(&manager);
    assert_eq!(busy.plugins, [format!("{PLUGIN}/personal")]);
    assert!(!busy.idle());
    manager.plugins().shutdown().await;
    assert!(tracon::restart::busy(&manager).idle());
}

#[tokio::test]
async fn a_process_left_by_a_node_killed_outright_is_ended_at_the_next_start() {
    use tracon::plugin::supervisor::running;
    let r = rig();
    r.sup.ensure(PLUGIN, "personal").await.unwrap();
    let pid = r.pid("personal");
    let records = || -> Vec<PathBuf> {
        std::fs::read_dir(running::dir(&r.state))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    };
    let recorded = records();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let record = recorded[0].clone();
    assert!(
        record
            .to_string_lossy()
            .ends_with(&format!(".personal.{pid}.json")),
        "{record:?}"
    );
    // As the next run of the node finds it: written by a run that is gone.
    let mut written: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    written["node"] = json!("a-run-killed-outright");
    std::fs::write(&record, written.to_string()).unwrap();
    let ended = tokio::task::spawn_blocking({
        let state = r.state.clone();
        move || running::reap_orphans(&state, Duration::from_secs(2))
    })
    .await
    .unwrap();
    assert_eq!(ended, 1);
    r.until("the orphan ends", Duration::from_secs(5), |_| gone(pid))
        .await;
    assert!(!record.exists());
    // A process this run starts is recorded and, once stopped, forgotten.
    r.sup.shutdown().await;
    assert!(records().is_empty(), "{:?}", records());
}
