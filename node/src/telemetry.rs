//! What this node exposes for Prometheus on `[metrics] listen`: one
//! process-global registry, recorded into where the work happens and encoded
//! by `http::metrics` on each scrape. (`crate::metrics` is the JSON workflow
//! metrics the operator API serves; this is the scrape target.)
//!
//! Label values come only from enums, the configured repositories, channels
//! and providers, and the model catalogue, so every series recurs. Never a
//! session id, pod name, review id, branch, prompt, path, error text or
//! credential: a value that never recurs is a series that never ends, and
//! per-session drill-down is already the cluster's, joining cAdvisor's per-pod
//! series with `kube_pod_labels`. Histograms carry fewer labels than counters.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration;

use prometheus_client::encoding::{EncodeLabelSet, EncodeMetric, MetricEncoder};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::metrics::MetricType;
use prometheus_client::registry::{Registry, Unit};

use crate::provider::kubernetes::pod::label_value;
use crate::runner::RunKind;

/// What a scrape is answered with.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

const POD_START_BUCKETS: &[f64] = &[
    0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 900.0,
];
const SESSION_READY_BUCKETS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0];
const SESSION_DURATION_BUCKETS: &[f64] = &[
    60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0, 14400.0, 28800.0, 86400.0,
];
const COMMAND_BUCKETS: &[f64] = &[
    5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0,
];
const IMAGE_BUILD_BUCKETS: &[f64] = &[
    10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0,
];

/// A repository's or a channel's name as a label value: what the pod labels
/// carry for it (`provider::kubernetes::pod::label_value`), empty (absent) when none.
pub fn name_label(name: Option<&str>) -> String {
    name.and_then(label_value).unwrap_or_default()
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PodReason {
    kind: String,
    sizing: String,
    reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PodSizing {
    kind: String,
    sizing: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct KindLabel {
    kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SessionState {
    state: String,
    phase: String,
    harness: String,
    repo: String,
    channel: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SessionStart {
    outcome: String,
    phase: String,
    harness: String,
    repo: String,
    channel: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SessionEnd {
    reason: String,
    phase: String,
    harness: String,
    repo: String,
    channel: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SessionReady {
    harness: String,
    image: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SessionDuration {
    harness: String,
    reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CheckLabels {
    origin: String,
    repo: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CheckDuration {
    origin: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PreparationLabels {
    purpose: String,
    repo: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PreparationDuration {
    purpose: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ImageBuild {
    image: String,
    repo: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ImageDuration {
    image: String,
    outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct GatewayCall {
    provider: String,
    model: String,
    channel: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct GatewayTokens {
    provider: String,
    model: String,
    channel: String,
    r#type: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildInfo {
    version: String,
    runtime: String,
}

/// A family whose `# HELP` and `# TYPE` are written before it has a series,
/// so a scrape shows every family this node reports and not only those
/// something has already happened to.
#[derive(Debug)]
struct Described<M>(M);

impl<M: EncodeMetric> EncodeMetric for Described<M> {
    fn encode(&self, encoder: MetricEncoder) -> Result<(), std::fmt::Error> {
        self.0.encode(encoder)
    }

    fn metric_type(&self) -> MetricType {
        self.0.metric_type()
    }
}

type Histograms<S> = Family<S, Histogram, fn() -> Histogram>;

fn pod_start_histogram() -> Histogram {
    Histogram::new(POD_START_BUCKETS.iter().copied())
}
fn session_ready_histogram() -> Histogram {
    Histogram::new(SESSION_READY_BUCKETS.iter().copied())
}
fn session_duration_histogram() -> Histogram {
    Histogram::new(SESSION_DURATION_BUCKETS.iter().copied())
}
fn command_histogram() -> Histogram {
    Histogram::new(COMMAND_BUCKETS.iter().copied())
}
fn image_build_histogram() -> Histogram {
    Histogram::new(IMAGE_BUILD_BUCKETS.iter().copied())
}

/// Every family this node exposes, and the registry that encodes them.
pub struct Telemetry {
    registry: Registry,
    pod_refusals: Family<PodReason, Counter>,
    pod_start_failures: Family<PodReason, Counter>,
    pod_start: Histograms<PodSizing>,
    runs_in_flight: Family<KindLabel, Gauge>,
    sessions: Family<SessionState, Gauge>,
    session_starts: Family<SessionStart, Counter>,
    session_ends: Family<SessionEnd, Counter>,
    session_ready: Histograms<SessionReady>,
    session_duration: Histograms<SessionDuration>,
    checks: Family<CheckLabels, Counter>,
    check_duration: Histograms<CheckDuration>,
    preparations: Family<PreparationLabels, Counter>,
    preparation_duration: Histograms<PreparationDuration>,
    image_builds: Family<ImageBuild, Counter>,
    image_build_duration: Histograms<ImageDuration>,
    gateway_requests: Family<GatewayCall, Counter>,
    gateway_tokens: Family<GatewayTokens, Counter>,
    build_info: Family<BuildInfo, Gauge>,
    /// The models a provider is configured with or offers, by provider: a
    /// model outside it is counted as `other`, so a harness cannot mint
    /// series by sending arbitrary model strings.
    catalogue: parking_lot::RwLock<BTreeMap<String, BTreeSet<String>>>,
}

/// The node's one registry. Runners are made per call, so nothing narrower
/// would outlive the run it counts.
pub fn telemetry() -> &'static Telemetry {
    static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();
    TELEMETRY.get_or_init(Telemetry::new)
}

impl Telemetry {
    fn new() -> Self {
        let mut registry = Registry::default();
        let pod_refusals = Family::<PodReason, Counter>::default();
        registry.register(
            "tracon_pod_refusals",
            "Harness pods the cluster refused for lack of capacity",
            Described(pod_refusals.clone()),
        );
        let pod_start_failures = Family::<PodReason, Counter>::default();
        registry.register(
            "tracon_pod_start_failures",
            "Harness pods that failed to start for a reason other than capacity",
            Described(pod_start_failures.clone()),
        );
        let pod_start = Histograms::<PodSizing>::new_with_constructor(pod_start_histogram);
        registry.register_with_unit(
            "tracon_pod_start",
            "Time from pod create to its container running (scheduling, image pull, volume mount)",
            Unit::Seconds,
            Described(pod_start.clone()),
        );
        let runs_in_flight = Family::<KindLabel, Gauge>::default();
        registry.register(
            "tracon_runs_in_flight",
            "Runner commands (session harnesses, checks, preparations, probes) this node is running now",
            Described(runs_in_flight.clone()),
        );
        let sessions = Family::<SessionState, Gauge>::default();
        registry.register(
            "tracon_sessions",
            "Sessions owned by this node in a non-terminal state",
            Described(sessions.clone()),
        );
        let session_starts = Family::<SessionStart, Counter>::default();
        registry.register(
            "tracon_session_starts",
            "Session starts, by whether the harness came up",
            Described(session_starts.clone()),
        );
        let session_ends = Family::<SessionEnd, Counter>::default();
        registry.register(
            "tracon_session_ends",
            "Sessions that ended, by why",
            Described(session_ends.clone()),
        );
        let session_ready =
            Histograms::<SessionReady>::new_with_constructor(session_ready_histogram);
        registry.register_with_unit(
            "tracon_session_ready",
            "Time from a session start request to the harness handshake completing",
            Unit::Seconds,
            Described(session_ready.clone()),
        );
        let session_duration =
            Histograms::<SessionDuration>::new_with_constructor(session_duration_histogram);
        registry.register_with_unit(
            "tracon_session_duration",
            "How long a session's harness ran, from the start request to end or suspension",
            Unit::Seconds,
            Described(session_duration.clone()),
        );
        let checks = Family::<CheckLabels, Counter>::default();
        registry.register(
            "tracon_checks",
            "Check commands finished",
            Described(checks.clone()),
        );
        let check_duration = Histograms::<CheckDuration>::new_with_constructor(command_histogram);
        registry.register_with_unit(
            "tracon_check_duration",
            "Wall time of a check command, excluding reused results",
            Unit::Seconds,
            Described(check_duration.clone()),
        );
        let preparations = Family::<PreparationLabels, Counter>::default();
        registry.register(
            "tracon_preparations",
            "Preparation runs finished",
            Described(preparations.clone()),
        );
        let preparation_duration =
            Histograms::<PreparationDuration>::new_with_constructor(command_histogram);
        registry.register_with_unit(
            "tracon_preparation_duration",
            "Wall time of a preparation, all steps",
            Unit::Seconds,
            Described(preparation_duration.clone()),
        );
        let image_builds = Family::<ImageBuild, Counter>::default();
        registry.register(
            "tracon_image_builds",
            "Repository image builds",
            Described(image_builds.clone()),
        );
        let image_build_duration =
            Histograms::<ImageDuration>::new_with_constructor(image_build_histogram);
        registry.register_with_unit(
            "tracon_image_build_duration",
            "Wall time of an image build including its probe",
            Unit::Seconds,
            Described(image_build_duration.clone()),
        );
        let gateway_requests = Family::<GatewayCall, Counter>::default();
        registry.register(
            "tracon_gateway_requests",
            "Model calls the gateway forwarded",
            Described(gateway_requests.clone()),
        );
        let gateway_tokens = Family::<GatewayTokens, Counter>::default();
        registry.register(
            "tracon_gateway_tokens",
            "Tokens the gateway counted on model calls",
            Described(gateway_tokens.clone()),
        );
        let build_info = Family::<BuildInfo, Gauge>::default();
        registry.register(
            "tracon_build_info",
            "Tracon version and runtime kind",
            Described(build_info.clone()),
        );
        Self {
            registry,
            pod_refusals,
            pod_start_failures,
            pod_start,
            runs_in_flight,
            sessions,
            session_starts,
            session_ends,
            session_ready,
            session_duration,
            checks,
            check_duration,
            preparations,
            preparation_duration,
            image_builds,
            image_build_duration,
            gateway_requests,
            gateway_tokens,
            build_info,
            catalogue: Default::default(),
        }
    }

    /// Everything, in the exposition format `CONTENT_TYPE` names.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        // Writing to a String does not fail.
        let _ = prometheus_client::encoding::text::encode(&mut out, &self.registry);
        out
    }

    pub fn pod_refused(&self, kind: &str, sizing: &str, reason: &str) {
        self.pod_refusals
            .get_or_create(&PodReason {
                kind: kind.into(),
                sizing: sizing.into(),
                reason: reason.into(),
            })
            .inc();
    }

    /// `reason`: `image_pull`, `config_error`, `start_timeout`,
    /// `create_error` or `other`.
    pub fn pod_start_failed(&self, kind: &str, sizing: &str, reason: &str) {
        self.pod_start_failures
            .get_or_create(&PodReason {
                kind: kind.into(),
                sizing: sizing.into(),
                reason: reason.into(),
            })
            .inc();
    }

    pub fn pod_started(&self, kind: &str, sizing: &str, took: Duration) {
        self.pod_start
            .get_or_create(&PodSizing {
                kind: kind.into(),
                sizing: sizing.into(),
            })
            .observe(took.as_secs_f64());
    }

    /// Count a run of `kind` in flight until the guard is dropped.
    pub fn run_started(&'static self, kind: &str) -> InFlight {
        let labels = KindLabel { kind: kind.into() };
        self.runs_in_flight.get_or_create(&labels).inc();
        InFlight {
            telemetry: self,
            labels,
        }
    }

    /// Replace the session gauge with `counts`: computed from the store on
    /// each scrape, so it is right after a restart.
    pub fn set_sessions(&self, counts: impl IntoIterator<Item = (SessionSeries, String, i64)>) {
        self.sessions.clear();
        for (series, state, count) in counts {
            self.sessions
                .get_or_create(&SessionState {
                    state,
                    phase: series.phase,
                    harness: series.harness,
                    repo: series.repo,
                    channel: series.channel,
                })
                .set(count);
        }
    }

    pub fn session_started(&self, series: &SessionSeries, image: &str, ready: Duration) {
        self.session_starts
            .get_or_create(&series.start("started"))
            .inc();
        self.session_ready
            .get_or_create(&SessionReady {
                harness: series.harness.clone(),
                image: image.into(),
            })
            .observe(ready.as_secs_f64());
    }

    pub fn session_start_failed(&self, series: &SessionSeries) {
        self.session_starts
            .get_or_create(&series.start("failed"))
            .inc();
    }

    /// A session ended for `reason` (`EndReason::as_str`); `ran` is how long
    /// its harness held its pod, when one was running.
    pub fn session_ended(&self, series: &SessionSeries, reason: &str, ran: Option<Duration>) {
        self.session_ends
            .get_or_create(&SessionEnd {
                reason: reason.into(),
                phase: series.phase.clone(),
                harness: series.harness.clone(),
                repo: series.repo.clone(),
                channel: series.channel.clone(),
            })
            .inc();
        if let Some(ran) = ran {
            self.session_ran(series, reason, ran);
        }
    }

    /// A session's harness stopped holding its pod for `reason`: its end, or
    /// `suspended`.
    pub fn session_ran(&self, series: &SessionSeries, reason: &str, ran: Duration) {
        self.session_duration
            .get_or_create(&SessionDuration {
                harness: series.harness.clone(),
                reason: reason.into(),
            })
            .observe(ran.as_secs_f64());
    }

    /// A check command finished. `took` is `None` for a reused result, which
    /// ran nothing.
    pub fn check_finished(
        &self,
        origin: &str,
        repo: Option<&str>,
        outcome: &str,
        took: Option<Duration>,
    ) {
        self.checks
            .get_or_create(&CheckLabels {
                origin: origin.into(),
                repo: name_label(repo),
                outcome: outcome.into(),
            })
            .inc();
        if let Some(took) = took {
            self.check_duration
                .get_or_create(&CheckDuration {
                    origin: origin.into(),
                    outcome: outcome.into(),
                })
                .observe(took.as_secs_f64());
        }
    }

    /// `outcome`: `prepared`, `failed`, `timed_out` or `cancelled`.
    pub fn preparation_finished(
        &self,
        purpose: Purpose,
        repo: Option<&str>,
        outcome: &str,
        took: Duration,
    ) {
        self.preparations
            .get_or_create(&PreparationLabels {
                purpose: purpose.as_str().into(),
                repo: name_label(repo),
                outcome: outcome.into(),
            })
            .inc();
        self.preparation_duration
            .get_or_create(&PreparationDuration {
                purpose: purpose.as_str().into(),
                outcome: outcome.into(),
            })
            .observe(took.as_secs_f64());
    }

    /// `image`: `base` or `session`; `outcome`: `built`, `failed` or
    /// `transient`.
    pub fn image_built(&self, image: &str, repo: Option<&str>, outcome: &str, took: Duration) {
        self.image_builds
            .get_or_create(&ImageBuild {
                image: image.into(),
                repo: name_label(repo),
                outcome: outcome.into(),
            })
            .inc();
        self.image_build_duration
            .get_or_create(&ImageDuration {
                image: image.into(),
                outcome: outcome.into(),
            })
            .observe(took.as_secs_f64());
    }

    /// The models each provider is configured with: what a recorded model
    /// is checked against.
    pub fn set_catalogue(&self, cfg: &crate::config::Config) {
        let catalogue = cfg
            .providers
            .iter()
            .map(|(name, provider)| {
                (
                    name.clone(),
                    provider.models.iter().map(|m| m.id.clone()).collect(),
                )
            })
            .collect();
        *self.catalogue.write() = catalogue;
    }

    /// `model` when `provider`'s catalogue has it, else `other`.
    fn model_label(&self, provider: &str, model: Option<&str>) -> String {
        let known = model.filter(|model| {
            self.catalogue
                .read()
                .get(provider)
                .is_some_and(|models| models.contains(*model))
        });
        known.unwrap_or("other").to_string()
    }

    /// A model call the gateway forwarded, and the tokens counted on it.
    pub fn model_call(&self, row: &crate::store::UsageRow) {
        let call = GatewayCall {
            provider: row.provider.clone(),
            model: self.model_label(&row.provider, row.model.as_deref()),
            channel: name_label(Some(&row.channel)),
        };
        self.gateway_requests
            .get_or_create(&call)
            .inc_by(row.requests.max(0) as u64);
        for (kind, count) in [
            ("input", row.input_tokens),
            ("output", row.output_tokens),
            ("cache_read", row.cache_read_tokens),
            ("cache_write", row.cache_write_tokens),
        ] {
            self.gateway_tokens
                .get_or_create(&GatewayTokens {
                    provider: call.provider.clone(),
                    model: call.model.clone(),
                    channel: call.channel.clone(),
                    r#type: kind.into(),
                })
                .inc_by(count.max(0) as u64);
        }
    }

    pub fn set_build_info(&self, version: &str, runtime: &str) {
        self.build_info
            .get_or_create(&BuildInfo {
                version: version.into(),
                runtime: runtime.into(),
            })
            .set(1);
    }
}

/// A run counted in `tracon_runs_in_flight` until dropped.
pub struct InFlight {
    telemetry: &'static Telemetry,
    labels: KindLabel,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.telemetry
            .runs_in_flight
            .get_or_create(&self.labels)
            .dec();
    }
}

/// The kind a run is counted under: what the caller said it is, else how it
/// is sized, as its pod's `tracon.dev/kind` label falls back.
pub fn run_kind(kind: Option<RunKind>, sizing: &'static str) -> &'static str {
    kind.map(RunKind::as_str).unwrap_or(sizing)
}

/// The repository a session's series name: the directory of the
/// repository its work came from (`environment::origin_repo`), as its pod's
/// `tracon.dev/repo` label names it. A continued session's own path is
/// `workspace://<session>`, which would make a series per session; it is
/// followed back to its origin, and never named itself.
pub fn session_repo(store: &crate::store::Store, session_id: &str) -> Option<String> {
    let origin = crate::environment::origin_repo(store, Some(session_id))
        .ok()
        .flatten();
    crate::runner::repo_name(origin.as_deref())
}

/// The labels a session's series carry: its phase and harness, and its
/// repository's and channel's names as the pod labels write them.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct SessionSeries {
    pub phase: String,
    pub harness: String,
    pub repo: String,
    pub channel: String,
}

impl SessionSeries {
    pub fn new(phase: &str, harness: &str, repo: Option<&str>, channel: Option<&str>) -> Self {
        Self {
            phase: phase.into(),
            harness: harness.into(),
            repo: name_label(repo),
            channel: name_label(channel),
        }
    }

    /// The series of a session as its row records it, its repository
    /// named as `session_repo` names it.
    pub fn of_row(store: &crate::store::Store, row: &crate::store::SessionRow) -> Self {
        Self::new(
            &row.phase,
            &row.harness_id,
            session_repo(store, &row.id).as_deref(),
            Some(&row.channel),
        )
    }

    fn start(&self, outcome: &str) -> SessionStart {
        SessionStart {
            outcome: outcome.into(),
            phase: self.phase.clone(),
            harness: self.harness.clone(),
            repo: self.repo.clone(),
            channel: self.channel.clone(),
        }
    }
}

/// What a preparation was run for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A candidate's, before its checks.
    Check,
    /// A session's workspace, before its harness starts.
    Session,
    /// The default branch's, which fills the base cache.
    Warm,
    /// A repository setup trial's.
    Trial,
    /// `environment::prepare`.
    Environment,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Session => "session",
            Self::Warm => "warm",
            Self::Trial => "trial",
            Self::Environment => "environment",
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Every family metrics.md lists, as the exposition names it: a counter's
    /// samples carry `_total`, its family name does not.
    pub(crate) const FAMILIES: &[&str] = &[
        "tracon_pod_refusals",
        "tracon_pod_start_failures",
        "tracon_pod_start_seconds",
        "tracon_runs_in_flight",
        "tracon_sessions",
        "tracon_session_starts",
        "tracon_session_ends",
        "tracon_session_ready_seconds",
        "tracon_session_duration_seconds",
        "tracon_checks",
        "tracon_check_duration_seconds",
        "tracon_preparations",
        "tracon_preparation_duration_seconds",
        "tracon_image_builds",
        "tracon_image_build_duration_seconds",
        "tracon_gateway_requests",
        "tracon_gateway_tokens",
        "tracon_build_info",
    ];

    #[test]
    fn every_family_is_described_before_anything_is_recorded() {
        let body = Telemetry::new().encode();
        for family in FAMILIES {
            assert!(
                body.contains(&format!("# HELP {family} ")),
                "{family}\n{body}"
            );
            assert!(
                body.contains(&format!("# TYPE {family} ")),
                "{family}\n{body}"
            );
        }
        assert!(body.ends_with("# EOF\n"), "{body}");
    }

    /// A label value is quoted text: a quote, a backslash or a newline in it
    /// must not end it or start another line.
    #[test]
    fn a_label_value_is_escaped() {
        let telemetry = Telemetry::new();
        telemetry.set_build_info("1\"2\\3\n4", "podman");
        let body = telemetry.encode();
        assert!(
            body.contains(r#"tracon_build_info{version="1\"2\\3\n4",runtime="podman"} 1"#),
            "{body}"
        );
    }

    #[test]
    fn a_histogram_renders_its_buckets_sum_and_count() {
        let telemetry = Telemetry::new();
        telemetry.pod_started("check", "command", Duration::from_millis(1500));
        let body = telemetry.encode();
        for line in [
            r#"tracon_pod_start_seconds_bucket{le="1.0",kind="check",sizing="command"} 0"#,
            r#"tracon_pod_start_seconds_bucket{le="2.0",kind="check",sizing="command"} 1"#,
            r#"tracon_pod_start_seconds_bucket{le="+Inf",kind="check",sizing="command"} 1"#,
            r#"tracon_pod_start_seconds_sum{kind="check",sizing="command"} 1.5"#,
            r#"tracon_pod_start_seconds_count{kind="check",sizing="command"} 1"#,
        ] {
            assert!(body.contains(line), "{line}\n{body}");
        }
    }

    /// A harness names whatever model it likes; only the catalogue's become
    /// series of their own.
    #[test]
    fn a_model_outside_the_catalogue_is_counted_as_other() {
        let telemetry = Telemetry::new();
        let mut cfg = crate::config::Config::default();
        cfg.providers.insert(
            "anthropic".into(),
            crate::config::Provider {
                models: vec![
                    serde_json::from_value(serde_json::json!({ "id": "claude-opus-5" })).unwrap(),
                ],
                ..Default::default()
            },
        );
        telemetry.set_catalogue(&cfg);
        let row = |model: &str| crate::store::UsageRow {
            channel: "Work Stuff".into(),
            node_id: "n".into(),
            session_id: Some("s-0123".into()),
            provider: "anthropic".into(),
            model: Some(model.into()),
            at_ms: 0,
            input_tokens: 10,
            output_tokens: 5,
            requests: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            request_id: None,
        };
        telemetry.model_call(&row("claude-opus-5"));
        telemetry.model_call(&row("made-up-model-0123"));
        let body = telemetry.encode();
        assert!(
            body.contains(r#"tracon_gateway_requests_total{provider="anthropic",model="claude-opus-5",channel="work-stuff"} 1"#),
            "{body}"
        );
        assert!(
            body.contains(r#"tracon_gateway_tokens_total{provider="anthropic",model="other",channel="work-stuff",type="input"} 10"#),
            "{body}"
        );
        assert!(!body.contains("made-up-model"), "{body}");
        assert!(!body.contains("s-0123"), "{body}");
    }

    #[test]
    fn a_run_is_in_flight_until_its_guard_is_dropped() {
        let telemetry: &'static Telemetry = Box::leak(Box::new(Telemetry::new()));
        let guard = telemetry.run_started("check");
        assert!(telemetry
            .encode()
            .contains(r#"tracon_runs_in_flight{kind="check"} 1"#));
        drop(guard);
        assert!(telemetry
            .encode()
            .contains(r#"tracon_runs_in_flight{kind="check"} 0"#));
    }
}
