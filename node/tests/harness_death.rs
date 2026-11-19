//! A harness that dies in the middle of a turn, where it is not started
//! again: the node sees a dead harness, not a failed turn. It starts no other
//! turn on the dead handle, whatever is waiting to be sent, and ends the
//! session saying how the harness died, once the exit is in or a short wait
//! for it has passed. The supervisor is driven directly.

#[path = "support/mod.rs"]
mod support;

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Notify};

use support::rows::session_row;
use support::state;
use tracon::adapter::{
    AdapterError, ExitCause, HarnessCompat, HarnessEvent, HarnessHandle, TurnResult,
};
use tracon::runner::Runner;
use tracon::session::supervisor::{Command, Prompted, Supervisor};
use tracon::store::{now_ms, OperatorQuestionRow, Store};
use tracon::stream::Bus;

/// A harness process that dies under its first turn when told to. Any later
/// turn waits for ever, as one on a dead process's handle did: a second
/// prompt shows as a second `user_prompt` and nothing else.
struct Dying {
    prompts: Arc<StdMutex<Vec<String>>>,
    die: Arc<Notify>,
}

#[async_trait]
impl HarnessHandle for Dying {
    fn harness_session_id(&self) -> &str {
        "h-1"
    }
    fn compat(&self) -> HarnessCompat {
        HarnessCompat {
            agent: "claude".into(),
            version: "2.1.295".into(),
            protocol: "stream-json/1".into(),
        }
    }
    async fn prompt(&self, text: String) -> Result<TurnResult, AdapterError> {
        let first = {
            let mut prompts = self.prompts.lock().unwrap();
            prompts.push(text);
            prompts.len() == 1
        };
        if first {
            self.die.notified().await;
            return Err(AdapterError::HarnessDied(
                "the harness process died mid-turn".into(),
            ));
        }
        std::future::pending().await
    }
    async fn cancel(&self) -> Result<(), AdapterError> {
        Ok(())
    }
    async fn close(&self) -> Result<(), AdapterError> {
        Ok(())
    }
}

struct NoRunner;

#[async_trait]
impl Runner for NoRunner {
    async fn spawn(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        unreachable!("nothing is relaunched")
    }
    async fn run_capture(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        unreachable!("nothing is relaunched")
    }
    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

struct Rig {
    store: Arc<Store>,
    id: String,
    prompts: Arc<StdMutex<Vec<String>>>,
    die: Arc<Notify>,
    events: mpsc::Sender<HarnessEvent>,
    commands: mpsc::Sender<Command>,
}

impl Rig {
    /// A session on a harness with no relaunch, as Claude Code's is on a
    /// DigitalOcean sandbox.
    async fn start() -> Self {
        state::isolate();
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.ensure_peer_node("n1").unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let mut row = session_row(&id, "n1", "personal");
        row.harness_id = "claude".into();
        row.state = "starting".into();
        row.budget_tokens = 0;
        store.insert_session(&row).unwrap();
        let prompts = Arc::new(StdMutex::new(Vec::new()));
        let die = Arc::new(Notify::new());
        let (events_tx, events) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let sup = Supervisor::new(
            id.clone(),
            "n1".into(),
            store.clone(),
            Bus::new(),
            Arc::new(Dying {
                prompts: prompts.clone(),
                die: die.clone(),
            }),
            Instant::now(),
            Duration::from_secs(60),
            cmd_tx.clone(),
            Arc::new(NoRunner),
            "tracon-h-test".into(),
            Default::default(),
            "personal".into(),
        );
        tokio::spawn(sup.run(events, cmd_rx));
        let rig = Self {
            store,
            id,
            prompts,
            die,
            events: events_tx,
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
        for _ in 0..1500 {
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

    async fn prompt(&self, text: &str) -> Prompted {
        let (ack, done) = oneshot::channel();
        self.commands
            .send(Command::Prompt {
                text: text.into(),
                ack,
            })
            .await
            .unwrap();
        done.await.unwrap().unwrap()
    }

    /// Everything that would start a turn once the one running ends: a
    /// question the operator has answered and the session was not told,
    /// and a message the operator queued while it ran.
    async fn something_waits_to_be_sent(&self) {
        self.store
            .insert_operator_question(&OperatorQuestionRow {
                id: "q1".into(),
                session_id: Some(self.id.clone()),
                channel: "personal".into(),
                node_id: "n1".into(),
                request_key: Some("q1".into()),
                prompt: "which remote?".into(),
                choices_json: r#"["origin","fork"]"#.into(),
                state: "unanswered".into(),
                answer_json: None,
                created_ms: now_ms(),
                answered_ms: None,
            })
            .unwrap();
        self.store
            .answer_operator_question("q1", "\"origin\"")
            .unwrap();
        assert!(matches!(
            self.prompt("and then the docs").await,
            Prompted::Queued { .. }
        ));
    }

    /// The turn is in a long tool call, then the process dies under it.
    async fn dies_in_a_tool_call(&self) {
        self.events
            .send(HarnessEvent::ToolCall(tracon::adapter::types::ToolCall {
                tool_call_id: "toolu_1".into(),
                title: "Bash".into(),
                kind: Some("execute".into()),
                status: Some("in_progress".into()),
                raw_input: Some(json!({ "command": "cargo test --lib" })),
                content: vec![],
                locations: vec![],
                parent_tool_call_id: None,
            }))
            .await
            .unwrap();
        self.wait("the call", |r| !r.events("tool_call").is_empty())
            .await;
        self.die.notify_one();
    }
}

#[tokio::test]
async fn a_harness_that_dies_mid_turn_gets_no_new_turn_and_ends_saying_why() {
    let rig = Rig::start().await;
    assert_eq!(rig.prompt("run the tests").await, Prompted::Sent);
    rig.something_waits_to_be_sent().await;
    rig.dies_in_a_tool_call().await;

    // The turn is settled, and nothing else is sent while the exit is on
    // its way, however hard the session is nudged.
    rig.wait("the cut turn", |r| !r.events("turn_end").is_empty())
        .await;
    rig.commands.send(Command::WaitMoved).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(rig.prompts.lock().unwrap().len(), 1);
    assert_eq!(rig.events("user_prompt").len(), 1);
    assert_ne!(rig.state(), "closed");

    rig.events
        .send(HarnessEvent::Exited {
            code: Some(137),
            cause: ExitCause {
                signal: Some(9),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    rig.wait("the end", |r| r.state() == "closed").await;

    assert_eq!(rig.events("user_prompt").len(), 1);
    assert_eq!(rig.prompts.lock().unwrap().len(), 1);
    let ends = rig.events("turn_end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["stop_reason"], "interrupted");
    let row = rig.store.get_session(&rig.id).unwrap().unwrap();
    assert_eq!(row.end_reason.as_deref(), Some("harness_died"));
    let error = row.last_error.unwrap();
    assert!(error.contains("died mid-turn"), "{error}");
    assert!(error.contains("killed by SIGKILL (exit 137)"), "{error}");
    assert!(error.contains("`Bash`"), "{error}");
    assert!(error.contains("background"), "{error}");
    // The log's error says the same, with the fields it was made from.
    let said = rig
        .events("error")
        .into_iter()
        .find(|e| e["cause"].is_string())
        .expect("the death is recorded");
    assert_eq!(said["harness_exit_code"], 137);
    assert_eq!(said["signal"], 9);
    assert_eq!(said["oom_killed"], false);
    assert_eq!(said["last_tool_call"]["title"], "Bash");
    assert_eq!(said["error"], json!(error));
}

/// An exit that never comes (the sandbox stopped answering with its harness)
/// does not hold the session open: after a short wait it ends, and says the
/// status is unknown.
#[tokio::test]
async fn a_harness_whose_exit_never_comes_still_ends_the_session() {
    let rig = Rig::start().await;
    assert_eq!(rig.prompt("run the tests").await, Prompted::Sent);
    rig.something_waits_to_be_sent().await;
    rig.dies_in_a_tool_call().await;

    rig.wait("the end", |r| r.state() == "closed").await;
    assert_eq!(rig.events("user_prompt").len(), 1);
    assert_eq!(rig.events("turn_end").len(), 1);
    let row = rig.store.get_session(&rig.id).unwrap().unwrap();
    assert_eq!(row.end_reason.as_deref(), Some("harness_died"));
    let error = row.last_error.unwrap();
    assert!(error.contains("exit status unknown"), "{error}");
}
