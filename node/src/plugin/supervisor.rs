//! Running plugins on the host: one process per plugin and channel.
//!
//! An instance starts when a channel first needs the plugin (`ensure`, which
//! the brokered tool calls make) and stops once it has gone unused for its
//! `[[plugin]]` entry's idle timeout, when the plugin is updated, rolled back
//! or removed, and when the node shuts down.
//!
//! Each start runs the active build's `exec` from where the node installed
//! it, once that build still digests to what was approved, with
//! `--socket <path> --data <dir>` appended and an empty environment but for
//! `TRACON_PLUGIN_TOKEN` (fresh for each start, held only here),
//! `TRACON_NODE_KEY` and `TRACON_CONTRACT`. No credential is in it: they are
//! lent per call. The socket is in a 0700 directory of the node's own, the
//! data directory is `<state>/plugins/data/<plugin>/<channel>/`, 0700.
//!
//! It is ready once MCP `initialize` answers on its socket and its
//! `tools/list` names exactly the tools its manifest declares. A start that
//! exits or is not ready in time is tried again after a wait that doubles
//! from one second to a minute; one more failure after that leaves the
//! instance unhealthy until it is asked for again or restarted. A plugin
//! whose tools disagree with its manifest is unhealthy at once, and is not
//! started again for that build until it is restarted, updated or rolled
//! back. Its output is kept in memory and in a rotated file under
//! `<state>/plugins/logs/`.
//!
//! A call holds a `Lease` on its instance: while one is out, the instance is
//! not stopped for being idle, and its idle time counts from the end of the
//! last call. The node's identity key (`set_signer`) signs each call's
//! caller context (`super::context`), and its public half is what a plugin
//! is started with as `TRACON_NODE_KEY`.
//!
//! A plugin whose manifest names `node_methods` is also started with
//! `--node-socket <path>`: a socket the node serves for that start alone
//! (`super::host`), in the same private directory, through which it calls
//! the node back. It goes when the start ends.
//!
//! The restart loop has the shape of the sandbox's service supervisor
//! (`tracon_sandbox::inside::services`) without sharing it: that one spawns
//! through the sandbox's `Spawner` as another user and reports in protocol
//! events, restarts by policy, and never gives up.

use std::{
    collections::{HashMap, VecDeque},
    io::Write as _,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, BufReader},
    sync::watch,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{install, manifest::Manifest};
use crate::mcp::upstream::RemoteTool;
use crate::store::{plugins::REMOVED, Store};

/// The contract version a plugin is started under (`TRACON_CONTRACT`).
pub const CONTRACT: &str = "1";

/// The longest a Unix socket's path may be on macOS (`sun_path`, with its
/// terminating NUL); Linux allows 108.
const MAX_SOCKET_PATH: usize = 104;

/// A socket's file name: twelve hex digits and `.sock`.
const SOCKET_NAME_LEN: usize = 17;

/// How many of an instance's lines are kept in memory, across its restarts.
const RING_LINES: usize = 1000;

/// The longest line kept; a longer one is kept in cuts.
const MAX_LOG_LINE: usize = 4096;

/// How large a log file grows before it is rotated, and how many rotated
/// ones are kept.
const LOG_FILE_BYTES: u64 = 1 << 20;
const LOG_ROTATIONS: usize = 3;

/// How long each try at `initialize` may take while an instance starts, and
/// how long between tries.
const READY_TRY: Duration = Duration::from_secs(2);
const READY_POLL: Duration = Duration::from_millis(100);

/// How long a starting instance has to list its tools.
const LIST_WITHIN: Duration = Duration::from_secs(10);

/// How the supervisor paces an instance.
#[derive(Debug, Clone)]
pub struct Timings {
    /// How long a start has to answer `initialize`.
    pub ready_within: Duration,
    /// The first wait before a restart, doubling up to `backoff_max`.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// A run ready this long starts the backoff over.
    pub reset_after: Duration,
    /// How long a stopped instance has after SIGTERM before SIGKILL.
    pub stop_grace: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            ready_within: Duration::from_secs(30),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            reset_after: Duration::from_secs(300),
            stop_grace: Duration::from_secs(5),
        }
    }
}

/// The wait before the next start after a failed one: `previous` is the
/// wait before the start that failed, `None` after a fresh start. `None`
/// when the backoff had reached its longest: the instance is unhealthy.
fn next_wait(previous: Option<Duration>, ready_for: Duration, t: &Timings) -> Option<Duration> {
    let max = t.backoff_max.max(t.backoff_initial);
    match previous {
        _ if ready_for >= t.reset_after => Some(t.backoff_initial),
        None => Some(t.backoff_initial),
        Some(wait) if wait >= max => None,
        Some(wait) => Some((wait * 2).min(max)),
    }
}

/// How long a plugin may sit unused, by name.
pub type IdleTimeout = Arc<dyn Fn(&str) -> Duration + Send + Sync>;

/// What an instance is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Starting,
    Ready,
    /// Waiting before it is started again.
    BackingOff,
    /// It failed with the backoff at its longest, or cannot start at all;
    /// not tried again until it is asked for or restarted.
    Unhealthy,
    Stopped,
}

impl State {
    fn live(self) -> bool {
        matches!(self, State::Starting | State::Ready | State::BackingOff)
    }
}

/// Where a ready instance is reached, and the token every call to it
/// carries.
#[derive(Clone)]
pub struct Endpoint {
    pub socket: PathBuf,
    pub token: Arc<str>,
    /// The archive digest of the build it runs.
    pub digest: String,
    /// That build's manifest: what a call to it is lent is read from this,
    /// not from whatever is active by the time the call is made.
    pub manifest: Arc<Manifest>,
    /// What it listed when it started, which its manifest declares.
    pub tools: Arc<[RemoteTool]>,
}

/// The tools a plugin's build listed the last time an instance of it
/// started, kept while no instance runs.
#[derive(Debug, Clone)]
pub struct Listed {
    pub digest: String,
    pub tools: Arc<[RemoteTool]>,
}

/// An instance held for one call: it is not stopped for being idle while a
/// lease on it is out, and its idle time counts from when the last one was
/// given back.
pub struct Lease {
    pub endpoint: Endpoint,
    instance: Arc<Instance>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.instance.touch();
        self.instance.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("socket", &self.socket)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

/// One instance as the operator sees it.
#[derive(Debug, Clone, Serialize)]
pub struct InstanceStatus {
    pub plugin: String,
    pub channel: String,
    pub state: State,
    pub pid: Option<u32>,
    /// When its process started, while it runs.
    pub started_ms: Option<i64>,
    pub uptime_secs: Option<u64>,
    /// How many times it was started again after a failure.
    pub restarts: u32,
    /// While it backs off, how long the wait is.
    pub backoff_ms: Option<u64>,
    pub last_error: Option<String>,
    pub version: Option<String>,
    pub digest: Option<String>,
}

#[derive(Default)]
struct Inner {
    pid: Option<u32>,
    started: Option<(Instant, i64)>,
    restarts: u32,
    backoff: Option<Duration>,
    last_error: Option<String>,
    version: Option<String>,
    digest: Option<String>,
    endpoint: Option<Endpoint>,
}

struct Run {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

struct Instance {
    plugin: String,
    channel: String,
    state: watch::Sender<State>,
    inner: std::sync::Mutex<Inner>,
    last_used: std::sync::Mutex<Instant>,
    /// Calls being made to it now (`Lease`).
    in_flight: AtomicUsize,
    /// Held while it is started or stopped.
    run: tokio::sync::Mutex<Option<Run>>,
    log: Log,
}

impl Instance {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elapsed()
    }

    fn set(&self, state: State) {
        self.state.send_replace(state);
    }

    fn fail(&self, why: String) {
        self.log.line(&format!("[tracon] {why}"));
        self.inner().last_error = Some(why);
    }

    fn status(&self) -> InstanceStatus {
        let inner = self.inner();
        InstanceStatus {
            plugin: self.plugin.clone(),
            channel: self.channel.clone(),
            state: *self.state.borrow(),
            pid: inner.pid,
            started_ms: inner.started.map(|(_, ms)| ms),
            uptime_secs: inner.started.map(|(at, _)| at.elapsed().as_secs()),
            restarts: inner.restarts,
            backoff_ms: inner.backoff.map(|d| d.as_millis() as u64),
            last_error: inner.last_error.clone(),
            version: inner.version.clone(),
            digest: inner.digest.clone(),
        }
    }
}

/// What the supervisor and every run task share.
struct Shared {
    store: Arc<Store>,
    state_dir: PathBuf,
    node_key: String,
    idle: IdleTimeout,
    timings: Timings,
    /// The node's identity, which signs every call.
    signer: OnceLock<Arc<proto::keys::Identity>>,
    /// Plugins whose build's tools disagree with its manifest: by plugin,
    /// the build's digest and why. Not started again while that build is
    /// active, until it is restarted.
    refused: std::sync::Mutex<HashMap<String, (String, String)>>,
    /// What each plugin's build listed when it last started.
    listed: std::sync::Mutex<HashMap<String, Listed>>,
    /// What answers a plugin's calls on its node socket. Weak: the node's
    /// tools own the supervisor, through the session manager.
    host: OnceLock<std::sync::Weak<dyn super::host::Host>>,
}

impl Shared {
    /// The key a plugin is told to verify calls with: the one that signs
    /// them, once the node has one.
    fn node_key(&self) -> String {
        self.signer
            .get()
            .map(|identity| identity.node_id())
            .unwrap_or_else(|| self.node_key.clone())
    }

    fn refused(&self) -> std::sync::MutexGuard<'_, HashMap<String, (String, String)>> {
        self.refused.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn listed(&self) -> std::sync::MutexGuard<'_, HashMap<String, Listed>> {
        self.listed.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The node's plugin instances.
pub struct Supervisor {
    shared: Arc<Shared>,
    instances: std::sync::Mutex<HashMap<(String, String), Arc<Instance>>>,
    closed: AtomicBool,
}

/// Whether `name` is one safe path component.
fn component_ok(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

impl Supervisor {
    /// `node_key` is the node id: the lowercase hex of its ed25519 public
    /// key.
    pub fn new(store: Arc<Store>, state_dir: PathBuf, node_key: String, idle: IdleTimeout) -> Self {
        Self::with_timings(store, state_dir, node_key, idle, Timings::default())
    }

    pub fn with_timings(
        store: Arc<Store>,
        state_dir: PathBuf,
        node_key: String,
        idle: IdleTimeout,
        timings: Timings,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                store,
                state_dir,
                node_key,
                idle,
                timings,
                signer: OnceLock::new(),
                refused: Default::default(),
                listed: Default::default(),
                host: OnceLock::new(),
            }),
            instances: Default::default(),
            closed: AtomicBool::new(false),
        }
    }

    /// Sign plugin calls with `identity`, the node's own: the key behind
    /// its node id. Set once; a plugin started from now on is given its
    /// public half.
    pub fn set_signer(&self, identity: proto::keys::Identity) {
        let _ = self.shared.signer.set(Arc::new(identity));
    }

    /// Answer plugins' calls on their node sockets with `host`. Set once.
    pub fn set_host(&self, host: std::sync::Weak<dyn super::host::Host>) {
        let _ = self.shared.host.set(host);
    }

    /// The public half of the key that signs plugin calls, once there is one.
    pub(crate) fn verifying_key(&self) -> Option<ed25519_dalek::VerifyingKey> {
        self.shared
            .signer
            .get()
            .map(|identity| identity.verifying_key())
    }

    /// `bytes`, signed with the node's identity.
    pub(crate) fn sign(&self, bytes: &[u8]) -> Result<ed25519_dalek::Signature, String> {
        self.shared
            .signer
            .get()
            .map(|identity| identity.sign(bytes))
            .ok_or_else(|| "this node cannot sign plugin calls".to_string())
    }

    /// What `plugin`'s build listed the last time an instance of it
    /// started.
    pub fn listed(&self, plugin: &str) -> Option<Listed> {
        self.shared.listed().get(plugin).cloned()
    }

    fn instances(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Arc<Instance>>> {
        self.instances.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn get(&self, plugin: &str, channel: &str) -> Option<Arc<Instance>> {
        self.instances()
            .get(&(plugin.to_string(), channel.to_string()))
            .cloned()
    }

    /// The instance of `plugin` for `channel`, started if it is not running,
    /// once it is ready. It counts as used now, which puts its idle stop
    /// off. An instance backing off is waited for, not hurried; an unhealthy
    /// or stopped one is started afresh.
    pub async fn ensure(&self, plugin: &str, channel: &str) -> Result<Endpoint, String> {
        let instance = self.instance(plugin, channel)?;
        self.serve(&instance).await
    }

    /// [`Self::ensure`], held for one call until the lease is dropped.
    pub async fn lease(&self, plugin: &str, channel: &str) -> Result<Lease, String> {
        let instance = self.instance(plugin, channel)?;
        // Held before it is served, so an idle stop cannot fall between.
        instance.in_flight.fetch_add(1, Ordering::SeqCst);
        let lease = |endpoint| Lease {
            endpoint,
            instance: instance.clone(),
        };
        match self.serve(&instance).await {
            Ok(endpoint) => Ok(lease(endpoint)),
            Err(error) => {
                instance.in_flight.fetch_sub(1, Ordering::SeqCst);
                Err(error)
            }
        }
    }

    /// The instance of `plugin` for `channel`, made if the node has none.
    fn instance(&self, plugin: &str, channel: &str) -> Result<Arc<Instance>, String> {
        if !component_ok(plugin) || !component_ok(channel) {
            return Err(format!(
                "{plugin:?} on {channel:?} is not a plugin instance"
            ));
        }
        Ok({
            let mut instances = self.instances();
            instances
                .entry((plugin.to_string(), channel.to_string()))
                .or_insert_with(|| {
                    Arc::new(Instance {
                        plugin: plugin.to_string(),
                        channel: channel.to_string(),
                        state: watch::channel(State::Stopped).0,
                        inner: Default::default(),
                        last_used: std::sync::Mutex::new(Instant::now()),
                        in_flight: AtomicUsize::new(0),
                        run: Default::default(),
                        log: Log::new(log_path(&self.shared.state_dir, plugin, channel)),
                    })
                })
                .clone()
        })
    }

    /// Why `plugin` is not started: its active build's tools disagreed with
    /// its manifest when it last started.
    fn refusal(&self, plugin: &str) -> Option<String> {
        let (digest, why) = self.shared.refused().get(plugin).cloned()?;
        let active = match self.shared.store.plugin_get(plugin) {
            Ok(Some(row)) if row.state != REMOVED => row.active.map(|b| b.digest),
            _ => None,
        };
        (active.as_deref() == Some(digest.as_str())).then_some(why)
    }

    /// `instance`, started if it is not running, once it is ready.
    async fn serve(&self, instance: &Arc<Instance>) -> Result<Endpoint, String> {
        let plugin = instance.plugin.as_str();
        if let Some(why) = self.refusal(plugin) {
            return Err(format!("plugin {plugin} is not started: {why}"));
        }
        instance.touch();
        // Once more if it stopped while this waited: an idle stop that began
        // before this use, or a stop for an update, which the next start
        // answers with what is active now.
        for attempt in 0..2 {
            {
                let mut run = instance.run.lock().await;
                if self.closed.load(Ordering::SeqCst) {
                    return Err("the node is shutting down".into());
                }
                if !instance.state.borrow().live() {
                    *run = Some(self.start(instance));
                }
            }
            match self.ready(instance).await {
                Err(None) if attempt == 0 => continue,
                Err(None) => break,
                Err(Some(error)) => return Err(error),
                Ok(endpoint) => return Ok(endpoint),
            }
        }
        let last = instance.inner().last_error.clone();
        Err(last.unwrap_or_else(|| format!("plugin {plugin} stopped")))
    }

    /// Wait for `instance` to be ready: its endpoint, or why it is not,
    /// `None` when it was stopped.
    async fn ready(&self, instance: &Instance) -> Result<Endpoint, Option<String>> {
        let mut state = instance.state.subscribe();
        let wait = async {
            loop {
                let now = *state.borrow_and_update();
                match now {
                    // Without an endpoint it is being stopped.
                    State::Ready => {
                        if let Some(endpoint) = instance.inner().endpoint.clone() {
                            return Ok(endpoint);
                        }
                    }
                    State::Unhealthy => {
                        return Err(Some(instance.inner().last_error.clone().unwrap_or_else(
                            || format!("plugin {} is unhealthy", instance.plugin),
                        )))
                    }
                    State::Stopped => return Err(None),
                    State::Starting | State::BackingOff => {}
                }
                if state.changed().await.is_err() {
                    return Err(None);
                }
            }
        };
        let within = self.shared.timings.ready_within + Duration::from_secs(5);
        match tokio::time::timeout(within, wait).await {
            Ok(outcome) => outcome,
            Err(_) => Err(Some(format!(
                "plugin {} is still starting on {}{}",
                instance.plugin,
                instance.channel,
                instance
                    .inner()
                    .last_error
                    .as_deref()
                    .map(|e| format!(" (last: {e})"))
                    .unwrap_or_default()
            ))),
        }
    }

    /// A fresh run task for `instance`, its state already `Starting`.
    fn start(&self, instance: &Arc<Instance>) -> Run {
        instance.set(State::Starting);
        {
            let mut inner = instance.inner();
            inner.backoff = None;
            inner.last_error = None;
        }
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(self.shared.clone(), instance.clone(), stop.clone()));
        Run { stop, task }
    }

    /// Stop `instance` if it runs, and wait until it has.
    async fn stop(instance: &Instance, why: &str) {
        let mut run = instance.run.lock().await;
        if let Some(run) = run.take() {
            if !run.task.is_finished() {
                instance.log.line(&format!("[tracon] stopping ({why})"));
            }
            run.stop.cancel();
            let _ = run.task.await;
        }
    }

    async fn stop_all(instances: Vec<Arc<Instance>>, why: &str) {
        // Every one is told first, so they halt together.
        for instance in &instances {
            if let Ok(run) = instance.run.try_lock() {
                if let Some(run) = run.as_ref() {
                    run.stop.cancel();
                }
            }
        }
        for instance in &instances {
            Self::stop(instance, why).await;
        }
    }

    /// Stop the instance of `plugin` on `channel` and start it afresh, its
    /// backoff cleared. Only an instance the node has had can be restarted.
    pub async fn restart(&self, plugin: &str, channel: &str) -> Result<Endpoint, String> {
        let instance = self
            .get(plugin, channel)
            .ok_or_else(|| format!("plugin {plugin} has no instance on {channel}"))?;
        Self::stop(&instance, "restart").await;
        self.shared.refused().remove(plugin);
        self.ensure(plugin, channel).await
    }

    /// Stop every instance of `name`: it was updated, rolled back or
    /// removed. The next `ensure` starts what is active then.
    pub async fn stop_plugin(&self, name: &str) {
        let instances: Vec<Arc<Instance>> = self
            .instances()
            .values()
            .filter(|i| i.plugin == name)
            .cloned()
            .collect();
        Self::stop_all(instances, "the plugin changed").await;
        self.shared.refused().remove(name);
        self.shared.listed().remove(name);
    }

    /// Stop every instance and start none again: the node is shutting down.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let instances: Vec<Arc<Instance>> = self.instances().values().cloned().collect();
        Self::stop_all(instances, "the node is shutting down").await;
    }

    /// Every instance the node has had since it started, by plugin and
    /// channel.
    pub fn status(&self) -> Vec<InstanceStatus> {
        let mut out: Vec<InstanceStatus> = self.instances().values().map(|i| i.status()).collect();
        out.sort_by(|a, b| (&a.plugin, &a.channel).cmp(&(&b.plugin, &b.channel)));
        out
    }

    /// `plugin/channel` for each instance running or about to: what a
    /// restart of the node waits for.
    pub fn running(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .instances()
            .values()
            .filter(|i| i.state.borrow().live())
            .map(|i| format!("{}/{}", i.plugin, i.channel))
            .collect();
        out.sort();
        out
    }

    /// The last `lines` lines of the instance's output: from memory while
    /// the node has it, else from its log file.
    pub fn logs(&self, plugin: &str, channel: &str, lines: usize) -> Vec<String> {
        if let Some(instance) = self.get(plugin, channel) {
            return instance.log.tail(lines);
        }
        if !component_ok(plugin) || !component_ok(channel) {
            return Vec::new();
        }
        let text = std::fs::read(log_path(&self.shared.state_dir, plugin, channel))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..]
            .iter()
            .map(|l| l.to_string())
            .collect()
    }
}

/// Why one start ended.
enum Outcome {
    /// It exited, or was not ready in time: tried again after a wait.
    Failed(String),
    /// It cannot start as it is: not tried again.
    Fatal(String),
    /// Unused for its idle timeout.
    Idle,
    Stopped,
}

/// An instance's life, from its start to a stop, an idle timeout, or
/// unhealthy.
async fn run(shared: Arc<Shared>, instance: Arc<Instance>, stop: CancellationToken) {
    let mut wait: Option<Duration> = None;
    loop {
        if stop.is_cancelled() {
            instance.set(State::Stopped);
            return;
        }
        instance.set(State::Starting);
        let (outcome, ready_for) = once(&shared, &instance, &stop).await;
        match outcome {
            Outcome::Stopped | Outcome::Idle => {
                if matches!(outcome, Outcome::Idle) {
                    instance.log.line("[tracon] stopped: idle");
                }
                instance.set(State::Stopped);
                return;
            }
            Outcome::Fatal(why) => {
                instance.fail(why);
                instance.set(State::Unhealthy);
                return;
            }
            Outcome::Failed(why) => instance.fail(why),
        }
        let Some(next) = next_wait(wait, ready_for, &shared.timings) else {
            instance.log.line("[tracon] unhealthy: not started again");
            instance.set(State::Unhealthy);
            return;
        };
        wait = Some(next);
        {
            let mut inner = instance.inner();
            inner.restarts += 1;
            inner.backoff = Some(next);
        }
        instance.set(State::BackingOff);
        tokio::select! {
            _ = tokio::time::sleep(next) => {}
            _ = stop.cancelled() => {
                instance.set(State::Stopped);
                return;
            }
        }
        instance.inner().backoff = None;
    }
}

/// One start of an instance, to its end, and how long it was ready.
async fn once(
    shared: &Shared,
    instance: &Instance,
    stop: &CancellationToken,
) -> (Outcome, Duration) {
    let plugin = instance.plugin.as_str();
    let build = match shared.store.plugin_get(plugin) {
        Ok(Some(row)) if row.state != REMOVED => row.active,
        Ok(_) => None,
        Err(e) => return (Outcome::Failed(e.to_string()), Duration::ZERO),
    };
    let Some(build) = build else {
        return (
            Outcome::Fatal(format!("plugin {plugin} is not installed")),
            Duration::ZERO,
        );
    };
    {
        let mut inner = instance.inner();
        inner.version = Some(build.version.clone());
        inner.digest = Some(build.digest.clone());
    }
    // Only what the node wrote and the operator approved runs.
    let (state_dir, name, checked) = (shared.state_dir.clone(), plugin.to_string(), build.clone());
    let verified =
        tokio::task::spawn_blocking(move || install::verify(&state_dir, &name, &checked))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
    if let Err(why) = verified {
        return (Outcome::Fatal(why), Duration::ZERO);
    }
    let data = match data_dir(&shared.state_dir, plugin, &instance.channel) {
        Ok(dir) => dir,
        Err(e) => {
            return (
                Outcome::Failed(format!("data directory: {e}")),
                Duration::ZERO,
            )
        }
    };
    let socket = match socket_dir() {
        Ok(dir) => dir.join(format!("{}.sock", hex::encode(rand::random::<[u8; 6]>()))),
        Err(e) => {
            return (
                Outcome::Failed(format!("socket directory: {e}")),
                Duration::ZERO,
            )
        }
    };
    let _ = std::fs::remove_file(&socket);
    let token: Arc<str> = hex::encode(rand::random::<[u8; 32]>()).into();
    // Held for this start alone: a later start is answered on another.
    let node_socket = if build.manifest.node_methods.is_empty() {
        None
    } else {
        let path =
            socket.with_file_name(format!("{}.sock", hex::encode(rand::random::<[u8; 6]>())));
        let from = super::host::From {
            plugin: plugin.to_string(),
            channel: instance.channel.clone(),
            token: token.clone(),
            version: build.version.clone(),
            manifest: Arc::new(build.manifest.clone()),
        };
        let host = shared
            .host
            .get()
            .cloned()
            .unwrap_or_else(|| std::sync::Weak::<NoHost>::new());
        match super::host::listen(path, from, host) {
            Ok(listening) => Some(listening),
            Err(e) => return (Outcome::Failed(format!("node socket: {e}")), Duration::ZERO),
        }
    };
    let dir = install::build_dir(&shared.state_dir, plugin, &build.version, &build.digest);
    let exec = &build.manifest.exec;
    // Reached through no link an agent could have made, and writable by no
    // one but the node.
    let program = match crate::config::path_guard::plugin_program(&dir.join(&exec[0])) {
        Ok(program) => program,
        Err(e) => return (Outcome::Fatal(e.to_string()), Duration::ZERO),
    };
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&exec[1..])
        .arg("--socket")
        .arg(&socket)
        .arg("--data")
        .arg(&data);
    if let Some(listening) = &node_socket {
        command.arg("--node-socket").arg(&listening.path);
    }
    command
        .env_clear()
        .env("TRACON_PLUGIN_TOKEN", &*token)
        .env("TRACON_NODE_KEY", shared.node_key())
        .env("TRACON_CONTRACT", CONTRACT)
        .current_dir(&data)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                Outcome::Fatal(format!("{}: {e}", program.display())),
                Duration::ZERO,
            )
        }
        Err(e) => return (Outcome::Failed(format!("start: {e}")), Duration::ZERO),
    };
    let pid = child.id().unwrap_or(0);
    instance
        .log
        .line(&format!("[tracon] started {} pid {pid}", build.version));
    {
        let mut inner = instance.inner();
        inner.pid = Some(pid);
        inner.started = Some((Instant::now(), crate::store::now_ms()));
    }
    let pumps = [
        child.stdout.take().map(|s| pump(s, instance.log.clone())),
        child.stderr.take().map(|s| pump(s, instance.log.clone())),
    ];
    let (exit_tx, mut exit) = watch::channel(None);
    tokio::spawn(async move {
        let status = child.wait().await.ok();
        let _ = exit_tx.send(Some(status));
    });

    let mut ready_at = None;
    let outcome = 'run: {
        let http = crate::mcp::upstream::unix_client(&socket);
        let ready = async {
            loop {
                if socket.exists()
                    && crate::mcp::upstream::initialize_unix(&http, READY_TRY)
                        .await
                        .is_ok()
                {
                    return;
                }
                tokio::time::sleep(READY_POLL).await;
            }
        };
        tokio::select! {
            status = exited(&mut exit) => break 'run Outcome::Failed(status),
            _ = stop.cancelled() => {
                halt(pid, &mut exit, &shared.timings).await;
                break 'run Outcome::Stopped;
            }
            ready = tokio::time::timeout(shared.timings.ready_within, ready) => {
                if ready.is_err() {
                    halt(pid, &mut exit, &shared.timings).await;
                    break 'run Outcome::Failed(format!(
                        "not ready within {}s: initialize did not answer on its socket",
                        shared.timings.ready_within.as_secs_f32()
                    ));
                }
            }
        }
        // What it serves is what its manifest declares, no more and no
        // less: the manifest is what the operator approved.
        let listed = tokio::select! {
            status = exited(&mut exit) => break 'run Outcome::Failed(status),
            _ = stop.cancelled() => {
                halt(pid, &mut exit, &shared.timings).await;
                break 'run Outcome::Stopped;
            }
            listed = crate::mcp::upstream::list_unix(&http, plugin, LIST_WITHIN) => listed,
        };
        let tools = match listed {
            Ok(tools) => tools,
            Err(why) => {
                halt(pid, &mut exit, &shared.timings).await;
                break 'run Outcome::Failed(why);
            }
        };
        if let Some(why) = disagreement(&build.manifest, &tools) {
            halt(pid, &mut exit, &shared.timings).await;
            shared
                .refused()
                .insert(plugin.to_string(), (build.digest.clone(), why.clone()));
            break 'run Outcome::Fatal(why);
        }
        let tools: Arc<[RemoteTool]> = tools.into();
        shared.listed().insert(
            plugin.to_string(),
            Listed {
                digest: build.digest.clone(),
                tools: tools.clone(),
            },
        );
        ready_at = Some(Instant::now());
        instance.inner().endpoint = Some(Endpoint {
            socket: socket.clone(),
            token: token.clone(),
            digest: build.digest.clone(),
            manifest: Arc::new(build.manifest.clone()),
            tools,
        });
        instance.log.line("[tracon] ready");
        // Its idle time counts from now: the use that started it is served
        // only now, however long the start took.
        instance.touch();
        instance.set(State::Ready);
        loop {
            let idle = (shared.idle)(plugin);
            // A call being made puts the stop off by a whole period; its end
            // starts the count again.
            let left = if instance.in_flight.load(Ordering::SeqCst) > 0 {
                idle
            } else {
                idle.saturating_sub(instance.idle_for())
            };
            tokio::select! {
                status = exited(&mut exit) => break 'run Outcome::Failed(status),
                _ = stop.cancelled() => {
                    halt(pid, &mut exit, &shared.timings).await;
                    break 'run Outcome::Stopped;
                }
                _ = tokio::time::sleep(left) => {
                    // A use since the wait began moved the deadline.
                    if instance.idle_for() >= (shared.idle)(plugin)
                        && instance.in_flight.load(Ordering::SeqCst) == 0
                    {
                        // A use from here on waits for the stop and starts
                        // it again, rather than being handed this process.
                        instance.inner().endpoint = None;
                        halt(pid, &mut exit, &shared.timings).await;
                        break 'run Outcome::Idle;
                    }
                }
            }
        }
    };
    {
        let mut inner = instance.inner();
        inner.endpoint = None;
        inner.pid = None;
        inner.started = None;
    }
    let _ = std::fs::remove_file(&socket);
    drop(node_socket);
    // Its last lines, unless something it started still holds the pipe.
    for pump in pumps.into_iter().flatten() {
        let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
    }
    (outcome, ready_at.map(|at| at.elapsed()).unwrap_or_default())
}

/// What answers a node socket before the node has set its host: nothing.
struct NoHost;

#[async_trait::async_trait]
impl super::host::Host for NoHost {
    async fn call(
        &self,
        _: &super::host::From,
        _: &str,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, super::host::Error> {
        Err(super::host::Error::failed(
            "the node cannot answer plugins yet",
        ))
    }
}

/// Why `tools`, what a start listed, is not what `manifest` declares.
fn disagreement(manifest: &Manifest, tools: &[RemoteTool]) -> Option<String> {
    let declared: std::collections::BTreeSet<&str> =
        manifest.tools.iter().map(|t| t.name.as_str()).collect();
    let listed: std::collections::BTreeSet<&str> =
        tools.iter().map(|t| t.remote.as_str()).collect();
    let extra: Vec<&str> = listed.difference(&declared).copied().collect();
    let missing: Vec<&str> = declared.difference(&listed).copied().collect();
    if extra.is_empty() && missing.is_empty() && listed.len() == tools.len() {
        return None;
    }
    let mut parts = Vec::new();
    if !extra.is_empty() {
        parts.push(format!("lists {} not declared", extra.join(", ")));
    }
    if !missing.is_empty() {
        parts.push(format!("declares {} not listed", missing.join(", ")));
    }
    if listed.len() != tools.len() {
        parts.push("lists a tool twice".into());
    }
    Some(format!(
        "its tools disagree with its manifest: {}",
        parts.join("; ")
    ))
}

/// How a process ended, in words.
async fn exited(exit: &mut watch::Receiver<Option<Option<std::process::ExitStatus>>>) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    let status = loop {
        if let Some(status) = *exit.borrow_and_update() {
            break status;
        }
        if exit.changed().await.is_err() {
            break None;
        }
    };
    match status {
        Some(s) if s.code().is_some() => format!("exited with code {}", s.code().unwrap_or(0)),
        Some(s) => format!("killed by signal {}", s.signal().unwrap_or(0)),
        None => "exited".into(),
    }
}

/// SIGTERM to its process group, SIGKILL after the grace; whatever it left
/// in the group goes too.
async fn halt(
    pid: u32,
    exit: &mut watch::Receiver<Option<Option<std::process::ExitStatus>>>,
    timings: &Timings,
) {
    if pid == 0 {
        return;
    }
    let group = pid as libc::pid_t;
    // SAFETY: signalling a process group the node made; no memory is shared.
    unsafe { libc::killpg(group, libc::SIGTERM) };
    let _ = tokio::time::timeout(timings.stop_grace, exited(exit)).await;
    // SAFETY: as above.
    unsafe { libc::killpg(group, libc::SIGKILL) };
    exited(exit).await;
}

fn pump<R: AsyncRead + Unpin + Send + 'static>(stream: R, log: Log) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            // At most one bounded line; a longer one is kept in cuts.
            let read = (&mut reader)
                .take(MAX_LOG_LINE as u64)
                .read_until(b'\n', &mut line)
                .await;
            match read {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    log.line(&String::from_utf8_lossy(&line));
                }
            }
        }
    })
}

/// `<state>/plugins/data/<plugin>/<channel>/`, made 0700 with the two
/// directories above it.
fn data_dir(state_dir: &Path, plugin: &str, channel: &str) -> std::io::Result<PathBuf> {
    let plugin_dir = install::data_dir(state_dir, plugin);
    let dir = plugin_dir.join(channel);
    std::fs::create_dir_all(&dir)?;
    for each in [
        plugin_dir.parent().unwrap_or(&plugin_dir),
        &plugin_dir,
        &dir,
    ] {
        std::fs::set_permissions(each, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn log_path(state_dir: &Path, plugin: &str, channel: &str) -> PathBuf {
    install::root(state_dir)
        .join("logs")
        .join(plugin)
        .join(format!("{channel}.log"))
}

/// The private directory plugin sockets are made in: `p` under the node's
/// runtime directory, or under `/tmp/tracon-<uid>/` when a socket's path
/// there would be too long for macOS.
pub fn socket_dir() -> std::io::Result<PathBuf> {
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
    let preferred = crate::config::Config::runtime_dir().join("p");
    if fits(&preferred) {
        return private_dir(&preferred, &base);
    }
    // SAFETY: geteuid cannot fail.
    let uid = unsafe { libc::geteuid() };
    let short = PathBuf::from("/tmp")
        .join(format!("tracon-{uid}"))
        .join("p");
    private_dir(&short, Path::new("/tmp"))
}

/// Remove the sockets a node killed outright left behind. The directory is
/// shared by every node this user runs (a scratch node beside the real
/// one), so only a socket nothing listens on any more goes; a live one is
/// another node's. Each start makes a fresh socket, so this is tidying, not
/// correctness: a stale file is never reused.
pub fn sweep_sockets() {
    if let Ok(dir) = socket_dir() {
        sweep(&dir);
    }
}

fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "sock") {
            continue;
        }
        let refused = matches!(
            std::os::unix::net::UnixStream::connect(&path),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
        );
        if refused {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether a socket in `dir` has a path short enough for every platform.
fn fits(dir: &Path) -> bool {
    dir.as_os_str().len() + 1 + SOCKET_NAME_LEN < MAX_SOCKET_PATH
}

/// `dir`, made if it has to be and 0700, once it and every directory between
/// it and `base` is a real directory the node's user owns that no one else
/// may write to: a directory someone else made first is refused.
fn private_dir(dir: &Path, base: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // SAFETY: geteuid cannot fail.
    let uid = unsafe { libc::geteuid() };
    let refuse = |path: &Path, why: &str| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{}: {why}", path.display()),
        )
    };
    let mut ancestors: Vec<&Path> = dir
        .ancestors()
        .take_while(|a| *a != base && a.starts_with(base))
        .collect();
    if ancestors.is_empty() {
        ancestors.push(dir);
    }
    for path in ancestors {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_dir() {
            return Err(refuse(path, "not a directory"));
        }
        if meta.uid() != uid {
            return Err(refuse(path, "owned by another user"));
        }
        if path == dir {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        } else if meta.mode() & 0o022 != 0 {
            return Err(refuse(path, "writable by other users"));
        }
    }
    Ok(dir.to_path_buf())
}

/// An instance's output: the recent lines in memory, every line in its
/// rotated file.
#[derive(Clone)]
struct Log(Arc<std::sync::Mutex<(VecDeque<String>, LogFile)>>);

impl Log {
    fn new(path: PathBuf) -> Self {
        Self(Arc::new(std::sync::Mutex::new((
            VecDeque::new(),
            LogFile::new(path, LOG_FILE_BYTES, LOG_ROTATIONS),
        ))))
    }

    fn line(&self, line: &str) {
        let mut held = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let (ring, file) = &mut *held;
        if ring.len() == RING_LINES {
            ring.pop_front();
        }
        ring.push_back(line.to_string());
        if let Err(e) = file.write(line) {
            tracing::debug!(path = %file.path.display(), error = %e, "could not write a plugin's log");
        }
    }

    fn tail(&self, lines: usize) -> Vec<String> {
        let held = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let ring = &held.0;
        ring.iter()
            .skip(ring.len().saturating_sub(lines))
            .cloned()
            .collect()
    }
}

/// A log file rotated at `max` bytes to `.1`, `.2`… keeping `keep`.
struct LogFile {
    path: PathBuf,
    max: u64,
    keep: usize,
    open: Option<(std::fs::File, u64)>,
}

impl LogFile {
    fn new(path: PathBuf, max: u64, keep: usize) -> Self {
        Self {
            path,
            max,
            keep,
            open: None,
        }
    }

    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn write(&mut self, line: &str) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let len = line.len() as u64 + 1;
        if self
            .open
            .as_ref()
            .is_some_and(|(_, size)| size + len > self.max)
        {
            self.open = None;
            for n in (1..self.keep).rev() {
                let _ = std::fs::rename(self.rotated(n), self.rotated(n + 1));
            }
            std::fs::rename(&self.path, self.rotated(1))?;
        }
        if self.open.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
                for each in [dir.parent(), Some(dir)].into_iter().flatten() {
                    std::fs::set_permissions(each, std::fs::Permissions::from_mode(0o700))?;
                }
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&self.path)?;
            let size = file.metadata()?.len();
            self.open = Some((file, size));
        }
        let (file, size) = self.open.as_mut().expect("opened above");
        writeln!(file, "{line}")?;
        *size += len;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_to_its_longest_then_gives_up() {
        let t = Timings::default();
        let mut wait = None;
        let mut waits = Vec::new();
        while let Some(next) = next_wait(wait, Duration::ZERO, &t) {
            waits.push(next.as_secs());
            wait = Some(next);
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60]);
        // A long ready run starts it over.
        assert_eq!(
            next_wait(Some(t.backoff_max), t.reset_after, &t),
            Some(t.backoff_initial)
        );
    }

    #[test]
    fn a_log_file_rotates_keeping_three() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/p/c.log");
        let mut file = LogFile::new(path.clone(), 100, 3);
        for n in 0..50 {
            file.write(&format!("line {n:04} ........")).unwrap();
        }
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, ["c.log", "c.log.1", "c.log.2", "c.log.3"]);
        for name in &names {
            let size = std::fs::metadata(path.parent().unwrap().join(name))
                .unwrap()
                .len();
            assert!(size <= 100, "{name}: {size}");
        }
        let last = std::fs::read_to_string(&path).unwrap();
        assert!(last.ends_with("line 0049 ........\n"), "{last}");
        // The oldest are gone.
        let all: String = names
            .iter()
            .map(|n| std::fs::read_to_string(path.parent().unwrap().join(n)).unwrap())
            .collect();
        assert!(!all.contains("line 0000"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn the_ring_keeps_the_latest_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::new(dir.path().join("x.log"));
        for n in 0..RING_LINES + 10 {
            log.line(&n.to_string());
        }
        let tail = log.tail(usize::MAX);
        assert_eq!(tail.len(), RING_LINES);
        assert_eq!(tail[0], "10");
        assert_eq!(
            log.tail(2),
            [(RING_LINES + 8).to_string(), (RING_LINES + 9).to_string()]
        );
    }

    #[test]
    fn a_socket_path_too_long_for_macos_does_not_fit() {
        assert!(fits(Path::new("/run/user/1000/tracon/p")));
        assert!(fits(Path::new(
            "/var/folders/xx/abcdefghijklmnopqrstuvw0000gn/T/tracon-501/tracon/p"
        )));
        let long = PathBuf::from("/").join("d".repeat(90));
        assert!(!fits(&long));
        let socket = Path::new("/run/user/1000/tracon/p").join("0123456789ab.sock");
        assert_eq!(socket.file_name().unwrap().len(), SOCKET_NAME_LEN);
    }

    #[test]
    fn a_private_dir_is_0700_and_refuses_a_symlink() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("tracon/p");
        private_dir(&dir, base.path()).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let elsewhere = tempfile::tempdir().unwrap();
        let link = base.path().join("link");
        std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();
        let refused = private_dir(&link.join("p"), base.path()).unwrap_err();
        assert!(refused.to_string().contains("not a directory"), "{refused}");

        // A parent anyone may write to could have `p` swapped under it.
        let open = base.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let refused = private_dir(&open.join("p"), base.path()).unwrap_err();
        assert!(
            refused.to_string().contains("writable by other"),
            "{refused}"
        );
    }

    #[test]
    fn a_private_dir_owned_by_another_user_is_refused() {
        // SAFETY: geteuid cannot fail.
        let refused = if unsafe { libc::geteuid() } == 0 {
            // Root makes one and gives it away.
            let base = tempfile::tempdir().unwrap();
            let theirs = base.path().join("theirs");
            std::fs::create_dir(&theirs).unwrap();
            std::os::unix::fs::chown(&theirs, Some(65534), Some(65534)).unwrap();
            private_dir(&theirs.join("p"), base.path()).unwrap_err()
        } else {
            // `/` is root's, not ours.
            private_dir(Path::new("/"), Path::new("/nonexistent")).unwrap_err()
        };
        assert!(refused.to_string().contains("another user"), "{refused}");
    }

    #[test]
    fn a_sweep_removes_only_sockets_nothing_listens_on() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live.sock");
        let stale = dir.path().join("stale.sock");
        let _listening = std::os::unix::net::UnixListener::bind(&live).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        let other = dir.path().join("note.txt");
        std::fs::write(&other, "").unwrap();
        sweep(dir.path());
        assert!(live.exists());
        assert!(!stale.exists());
        assert!(other.exists());
    }

    #[test]
    fn what_a_start_lists_must_be_what_its_manifest_declares() {
        let manifest = Manifest::parse(
            "name = \"p\"\nversion = \"1.0.0\"\ncontract = 1\nexec = [\"bin/p\"]\n\n\
             [[tool]]\nname = \"query\"\nhint = \"read\"\n\n\
             [[tool]]\nname = \"describe\"\nhint = \"read\"\n",
        )
        .unwrap();
        let tools = |names: &[&str]| -> Vec<RemoteTool> {
            names
                .iter()
                .map(|n| RemoteTool {
                    name: format!("p__{n}"),
                    remote: n.to_string(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                })
                .collect()
        };
        assert_eq!(
            disagreement(&manifest, &tools(&["describe", "query"])),
            None
        );
        assert_eq!(
            disagreement(&manifest, &tools(&["query", "drop", "exec"])).unwrap(),
            "its tools disagree with its manifest: lists drop, exec not declared; declares \
             describe not listed"
        );
        assert!(
            disagreement(&manifest, &tools(&["query", "describe", "query"]))
                .unwrap()
                .contains("lists a tool twice")
        );
    }

    #[test]
    fn only_a_single_component_names_an_instance() {
        for bad in ["", ".", "..", "a/b", "a\0"] {
            assert!(!component_ok(bad), "{bad:?}");
        }
        assert!(component_ok("personal"));
    }
}
