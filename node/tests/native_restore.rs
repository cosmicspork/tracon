//! A restorable harness whose process dies is started again on the same
//! session, and a turn it died in is settled and carried on. The supervisor is
//! driven directly, with an adapter that records what each relaunch was given.

#[path = "support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use support::rows::session_row;
use support::state;
use tracon::adapter::{
    AdapterError, HarnessAdapter, HarnessCompat, HarnessEvent, HarnessHandle, HarnessVersion,
    LaunchSpec, ModelOption, ProtocolSupport, Resume, TurnResult,
};
use tracon::runner::Runner;
use tracon::session::supervisor::{Command, Prompted, Relaunch, Supervisor};
use tracon::store::{now_ms, Store, UsageRow};
use tracon::stream::Bus;

/// One harness process: it takes prompts and never answers them, so the test
/// decides how each turn ends.
struct Process {
    prompts: Arc<StdMutex<Vec<String>>>,
}

#[async_trait]
impl HarnessHandle for Process {
    fn harness_session_id(&self) -> &str {
        "h-1"
    }
    fn compat(&self) -> HarnessCompat {
        HarnessCompat {
            agent: "tracon-agent".into(),
            version: "1.0.0".into(),
            protocol: "acp/1".into(),
        }
    }
    fn resume_point(&self) -> Option<Resume> {
        Some(Resume {
            harness_session_id: "h-1".into(),
            last_seq: 7,
        })
    }
    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError> {
        self.prompts.lock().unwrap().push(text);
        std::future::pending().await
    }
    async fn cancel(&self) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn close(&self) -> Result<(), AdapterError> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Restorable {
    /// What each relaunch was asked to carry on.
    resumed: Arc<StdMutex<Vec<Option<Resume>>>>,
    /// Each process's event channel, latest last.
    events: Arc<StdMutex<Vec<mpsc::Sender<HarnessEvent>>>>,
    prompts: Arc<StdMutex<Vec<String>>>,
}

#[async_trait]
impl HarnessAdapter for Restorable {
    fn id(&self) -> &'static str {
        "native"
    }
    fn pinned_version(&self) -> &str {
        "1.0.0"
    }
    fn protocol(&self) -> ProtocolSupport {
        ProtocolSupport {
            name: "acp",
            min: 1,
            max: 1,
        }
    }
    fn restorable(&self) -> bool {
        true
    }
    async fn version(&self, _r: &dyn Runner) -> Result<HarnessVersion, AdapterError> {
        unreachable!()
    }
    async fn probe_models(
        &self,
        _r: &dyn Runner,
        _w: &tracon::gateway::model::Wiring,
    ) -> Result<Vec<ModelOption>, AdapterError> {
        unreachable!()
    }
    async fn launch(
        &self,
        _r: &dyn Runner,
        spec: LaunchSpec,
    ) -> Result<(Box<dyn HarnessHandle>, mpsc::Receiver<HarnessEvent>), AdapterError> {
        self.resumed.lock().unwrap().push(spec.resume);
        let (tx, rx) = mpsc::channel(64);
        self.events.lock().unwrap().push(tx);
        Ok((
            Box::new(Process {
                prompts: self.prompts.clone(),
            }),
            rx,
        ))
    }
}

struct NoRunner;

#[async_trait]
impl Runner for NoRunner {
    async fn spawn(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        unreachable!("the adapter never spawns")
    }
    async fn run_capture(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        unreachable!("the adapter never spawns")
    }
    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

fn spec() -> LaunchSpec {
    LaunchSpec {
        cwd_in_runner: "/work".into(),
        model: "local/m".into(),
        container_name: "tracon-h-test".into(),
        harness_home: "/root".into(),
        mcp_servers: Vec::new(),
        tools: Vec::new(),
        env: Vec::new(),
        secret_env: Default::default(),
        system_prompt_file: None,
        resume: None,
    }
}

struct Rig {
    store: Arc<Store>,
    id: String,
    adapter: Restorable,
    commands: mpsc::Sender<Command>,
}

impl Rig {
    async fn start() -> Self {
        state::isolate();
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.ensure_peer_node("n1").unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let mut row = session_row(&id, "n1", "personal");
        row.harness_id = "native".into();
        row.state = "starting".into();
        row.budget_tokens = 0;
        store.insert_session(&row).unwrap();
        let adapter = Restorable::default();
        // The first process, as the manager would have launched it.
        let (handle, events) = adapter.launch(&NoRunner, spec()).await.unwrap();
        adapter.resumed.lock().unwrap().clear();
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let sup = Supervisor::new(
            id.clone(),
            "n1".into(),
            store.clone(),
            Bus::new(),
            Arc::from(handle),
            Instant::now(),
            Duration::from_secs(60),
            cmd_tx.clone(),
            Arc::new(NoRunner),
            "tracon-h-test".into(),
            Default::default(),
            "personal".into(),
        )
        .with_relaunch(Relaunch {
            adapter: Arc::new(adapter.clone()),
            spec: spec(),
        });
        tokio::spawn(sup.run(events, cmd_rx));
        let rig = Self {
            store,
            id,
            adapter,
            commands: cmd_tx,
        };
        rig.wait("running", |r| r.state() == "running").await;
        rig
    }

    fn state(&self) -> String {
        self.store.get_session(&self.id).unwrap().unwrap().state
    }

    fn events(&self, kind: &str) -> Vec<serde_json::Value> {
        self.store
            .events_after(&self.id, 0, 500)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload)
            .collect()
    }

    async fn wait(&self, what: &str, ready: impl Fn(&Self) -> bool) {
        for _ in 0..300 {
            if ready(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "waited for {what}; events: {:?}",
            self.store.events_after(&self.id, 0, 500)
        );
    }

    async fn prompt(&self, text: &str) {
        let (ack, done) = oneshot::channel();
        self.commands
            .send(Command::Prompt {
                text: text.into(),
                ack,
            })
            .await
            .unwrap();
        assert_eq!(done.await.unwrap(), Ok(Prompted::Sent));
    }

    /// The current process dies.
    async fn die(&self) {
        let tx = self.adapter.events.lock().unwrap().last().unwrap().clone();
        tx.send(HarnessEvent::Exited { code: Some(137) })
            .await
            .unwrap();
    }

    fn gateway_counted(&self, input: i64) {
        self.store
            .record_usage(&UsageRow {
                channel: "personal".into(),
                node_id: "n1".into(),
                session_id: Some(self.id.clone()),
                provider: "local".into(),
                model: None,
                at_ms: now_ms(),
                input_tokens: input,
                output_tokens: 0,
                requests: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                request_id: None,
            })
            .unwrap();
    }
}

#[tokio::test]
async fn a_harness_that_dies_mid_turn_is_started_again_and_carries_the_turn_on() {
    let rig = Rig::start().await;
    rig.prompt("do the thing").await;
    rig.gateway_counted(400);
    rig.die().await;

    rig.wait("the restart", |r| !r.events("harness_restarted").is_empty())
        .await;
    let restarted = &rig.events("harness_restarted")[0];
    assert_eq!(restarted["outcome"], "restored");
    assert_eq!(restarted["exit_code"], 137);
    assert_eq!(restarted["interrupted_turn"], true);
    assert_eq!(
        *rig.adapter.resumed.lock().unwrap(),
        [Some(Resume {
            harness_session_id: "h-1".into(),
            last_seq: 7,
        })]
    );
    // The cut turn is charged what the gateway saw, and said to be interrupted.
    let ends = rig.events("turn_end");
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["stop_reason"], "interrupted");
    assert_eq!(
        rig.store.get_session(&rig.id).unwrap().unwrap().tokens_used,
        400
    );
    // The work carries on, prompted by the node, on the new process.
    rig.wait("the continuation", |r| {
        r.adapter.prompts.lock().unwrap().len() == 2
    })
    .await;
    let prompts = rig.events("user_prompt");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1]["origin"], "node");
    assert!(prompts[1]["text"]
        .as_str()
        .unwrap()
        .contains("outcome unknown"));
    assert_eq!(rig.state(), "running");
}

#[tokio::test]
async fn the_turn_failing_before_the_exit_arrives_comes_to_the_same() {
    let rig = Rig::start().await;
    rig.prompt("do the thing").await;
    // The prompt's pipe broke first: the turn reports an error whose event
    // pump had already stopped.
    rig.commands
        .send(Command::TurnDone {
            turn_id: 1,
            kind: "error",
            payload: json!({
                "error": "peer closed",
                "event_barrier_error": "tracon-agent event pump stopped",
            }),
            tokens: 0,
        })
        .await
        .unwrap();
    rig.die().await;

    rig.wait("the continuation", |r| {
        r.adapter.prompts.lock().unwrap().len() == 2
    })
    .await;
    let ends = rig.events("turn_end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["stop_reason"], "interrupted");
    assert!(rig.events("error").is_empty(), "{:?}", rig.events("error"));
    assert_eq!(rig.events("harness_restarted")[0]["interrupted_turn"], true);
}

#[tokio::test]
async fn an_idle_harness_that_dies_is_started_again_without_a_prompt() {
    let rig = Rig::start().await;
    rig.die().await;

    rig.wait("the restart", |r| !r.events("harness_restarted").is_empty())
        .await;
    assert_eq!(
        rig.events("harness_restarted")[0]["interrupted_turn"],
        false
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rig.adapter.prompts.lock().unwrap().is_empty());
    assert!(rig.events("user_prompt").is_empty());
    assert_eq!(rig.state(), "running");
}

#[tokio::test]
async fn a_harness_that_keeps_dying_ends_the_session() {
    let rig = Rig::start().await;
    rig.prompt("do the thing").await;
    for restarts in 1..=2 {
        rig.die().await;
        rig.wait("a restart", |r| {
            r.events("harness_restarted").len() == restarts
        })
        .await;
    }
    rig.die().await;

    rig.wait("the end", |r| r.state() == "closed").await;
    let restarted = rig.events("harness_restarted");
    assert_eq!(restarted.len(), 3);
    assert_eq!(restarted[2]["outcome"], "refused");
    let row = rig.store.get_session(&rig.id).unwrap().unwrap();
    assert_eq!(row.end_reason.as_deref(), Some("harness_exit"));
    // Every cut turn was settled once.
    assert!(rig
        .events("turn_end")
        .iter()
        .all(|e| e["stop_reason"] == "interrupted"));
    assert_eq!(rig.events("turn_end").len(), 3);
}

/// Seen live: the last request's own usage, queued behind the turn's result,
/// was recorded seven milliseconds after the turn settled without it.
#[tokio::test]
async fn a_requests_usage_queued_before_the_turn_ends_is_settled_with_it() {
    let rig = Rig::start().await;
    rig.prompt("do the thing").await;
    rig.store
        .record_usage(&UsageRow {
            channel: "personal".into(),
            node_id: "n1".into(),
            session_id: Some(rig.id.clone()),
            provider: "local".into(),
            model: None,
            at_ms: now_ms(),
            input_tokens: 400,
            output_tokens: 20,
            requests: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            request_id: Some("chatcmpl-1".into()),
        })
        .unwrap();
    let tx = rig.adapter.events.lock().unwrap().last().unwrap().clone();
    tx.send(HarnessEvent::RequestUsage(
        tracon::adapter::types::RequestUsage {
            request_id: Some("chatcmpl-1".into()),
            input_tokens: 400,
            output_tokens: 20,
            cached_read_tokens: 0,
        },
    ))
    .await
    .unwrap();
    rig.commands
        .send(Command::TurnDone {
            turn_id: 1,
            kind: "turn_end",
            payload: json!({ "stop_reason": "end_turn" }),
            tokens: 420,
        })
        .await
        .unwrap();

    rig.wait("the turn to settle", |r| !r.events("turn_end").is_empty())
        .await;
    let ledger = rig.store.turn_ledger(&rig.id, 10).unwrap();
    assert_eq!(ledger[0].state, "reconciled");
    assert_eq!(ledger[0].requests_matched, Some(1));
    assert_eq!(ledger[0].requests_unmatched, Some(0));
}

/// Not a restart, but the same harness's other way of carrying a session on:
/// a compaction it reports is recorded with the summary it now works from.
#[tokio::test]
async fn a_compaction_the_harness_reports_is_recorded() {
    let rig = Rig::start().await;
    let tx = rig.adapter.events.lock().unwrap().last().unwrap().clone();
    tx.send(HarnessEvent::Other(json!({
        "sessionUpdate": "context_compacted",
        "trigger": "threshold",
        "tokensBefore": 49_800,
        "tokensAfter": 6_200,
        "prunedResults": 0,
        "summarisedMessages": 34,
        "firstKeptCall": "call_9",
        "summary": "fixed the validator; tests pass",
        "contextWindow": 65_536,
    })))
    .await
    .unwrap();

    rig.wait("the compaction", |r| {
        !r.events("context_compacted").is_empty()
    })
    .await;
    let c = &rig.events("context_compacted")[0];
    assert_eq!(c["trigger"], "threshold");
    assert_eq!(c["tokens_before"], 49_800);
    assert_eq!(c["tokens_after"], 6_200);
    assert_eq!(c["summarised_messages"], 34);
    assert_eq!(c["first_kept_call"], "call_9");
    assert_eq!(c["summary"], "fixed the validator; tests pass");
    assert!(rig.events("error").is_empty());
}

/// A subagent's call is recorded under the spawn that started it, and so is
/// the request to run it, so the log can nest both.
#[tokio::test]
async fn a_subagents_calls_are_recorded_under_its_spawn() {
    let rig = Rig::start().await;
    let tx = rig.adapter.events.lock().unwrap().last().unwrap().clone();
    let call = |id: &str, title: &str, parent: Option<&str>| tracon::adapter::types::ToolCall {
        tool_call_id: id.into(),
        title: title.into(),
        kind: Some("read".into()),
        status: Some("pending".into()),
        raw_input: Some(json!({"path": "src/lib.rs"})),
        content: vec![],
        locations: vec![],
        parent_tool_call_id: parent.map(str::to_string),
    };
    tx.send(HarnessEvent::ToolCall(call("c1", "subagent: look", None)))
        .await
        .unwrap();
    tx.send(HarnessEvent::ToolCall(call(
        "c1/c2",
        "[look] read src/lib.rs",
        Some("c1"),
    )))
    .await
    .unwrap();
    // This rig's policy has no rules, so the read is put to the operator.
    let (reply, _answer) = oneshot::channel();
    let mut read = tracon::adapter::PermissionRequest::managed(
        Some("c1/c2".into()),
        "[look] read src/lib.rs".into(),
        "read",
        Some("src/lib.rs".into()),
        None,
        None,
        Vec::new(),
    );
    read.parent_tool_call_id = Some("c1".into());
    tx.send(HarnessEvent::Permission {
        request: read,
        reply,
    })
    .await
    .unwrap();

    rig.wait("the child's ask", |r| {
        !r.events("permission_request").is_empty()
    })
    .await;
    let calls = rig.events("tool_call");
    assert_eq!(calls[0]["parent"], serde_json::Value::Null);
    assert_eq!(calls[1]["parent"], "c1");
    let ask = &rig.events("permission_request")[0];
    assert_eq!(ask["tool_call_id"], "c1/c2");
    assert_eq!(ask["parent"], "c1");
    assert_eq!(ask["title"], "[look] read src/lib.rs");
}
