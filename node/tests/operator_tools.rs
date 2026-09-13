//! The three ways an agent reaches the operator, and what each one is
//! allowed to cost. Reporting an issue is a draft the operator reads when
//! they like; a notification is a ping with a link; only a question stops the
//! agent and joins the queue of things waiting on a person. The separation is
//! the subject: none of the first two may pause a session, mark it blocked,
//! or be counted as something the operator owes an answer to.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tracon::{
    broker::Broker,
    config::Config,
    mcp::{CallContext, SessionAccess, Tools},
    session::Manager,
    store::{now_ms, PushSubscriptionRow, Store},
    stream::Bus,
};

/// A p256 public key and auth secret a browser would have registered.
const KEY: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

struct Rig {
    store: Arc<Store>,
    tools: Arc<Tools>,
    ctx: CallContext,
}

fn rig() -> Rig {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.channel_put("personal", &[], "{}").unwrap();
    store
        .insert_session(&support::rows::session_row("s1", "n1", "personal"))
        .unwrap();
    let cfg = Arc::new(Config::default());
    let tools = Arc::new(Tools {
        broker: Broker::default().shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg,
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools.session.set(SessionAccess {
        store: store.clone(),
        manager,
    });
    Rig {
        store,
        tools,
        ctx: CallContext {
            session_id: "s1".into(),
            channel: "personal".into(),
            node_id: "n1".into(),
        },
    }
}

impl Rig {
    async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        self.tools.call(&self.ctx, name, &args).await
    }
    fn state(&self) -> String {
        self.store.get_session("s1").unwrap().unwrap().state
    }
    fn kinds(&self) -> Vec<String> {
        self.store
            .events_after("s1", 0, 200)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }
    /// A live device whose endpoint refuses the connection: the point is that
    /// an attempt is recorded, not that a phone lit up.
    fn subscribe(&self) {
        self.store
            .push_subscription_upsert(&PushSubscriptionRow {
                id: "dev-1".into(),
                session_hash: None,
                endpoint: "http://127.0.0.1:1/push/dev-1".into(),
                p256dh: KEY.into(),
                auth: AUTH.into(),
                user_agent: Some("test".into()),
                created_ms: now_ms(),
                last_ok_ms: None,
                fail_count: 0,
            })
            .unwrap();
    }
}

/// Reporting is how the agent says something about tracon itself. It is not
/// how it says it is stuck: the draft lands, the operator reads it when they
/// like, and the session carries on without waiting for anyone.
#[tokio::test]
async fn reporting_an_issue_leaves_the_session_running_and_the_queue_empty() {
    state::isolate();
    let rig = rig();
    let out = rig
        .call(
            "report_issue",
            json!({
                "title": "the worktree lock outlived the session",
                "expected": "the lock is released at teardown",
                "actual": "the next session refused to start",
            }),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "draft");
    assert!(out["issue_id"].as_str().is_some_and(|id| !id.is_empty()));

    assert_eq!(rig.state(), "running", "a report is not a pause");
    assert!(
        rig.store.open_operator_questions().unwrap().is_empty(),
        "a report is not a question"
    );
    assert!(
        !rig.kinds()
            .iter()
            .any(|kind| kind == "session_paused" || kind == "state"),
        "a report must not move the session: {:?}",
        rig.kinds()
    );
    let drafts = rig.store.issue_drafts(false).unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].state, "draft");
    assert!(drafts[0].published_url.is_none());
}

/// A notification is delivered and recorded as a delivery, not filed as
/// something the operator owes an answer to.
#[tokio::test]
async fn notifying_records_a_delivery_and_never_a_question() {
    state::isolate();
    let rig = rig();
    rig.subscribe();
    let out = rig
        .call(
            "notify_operator",
            json!({ "title": "checks are green", "message": "ready whenever you are" }),
        )
        .await
        .unwrap();
    assert_eq!(out["deduplicated"], false);
    let id = out["notification_id"].as_str().unwrap().to_string();
    assert_eq!(
        rig.store.notification_attempts(&id).unwrap().len(),
        1,
        "the attempt on the subscribed device is recorded"
    );
    // The tool is honest about what an attempt proves, and says nothing about
    // an answer, because nothing is being asked.
    let receipt = out["receipt"].as_str().unwrap_or_default().to_lowercase();
    for phrasing in ["answer", "question", "reply"] {
        assert!(!receipt.contains(phrasing), "{receipt}");
    }

    assert!(
        rig.store.open_operator_questions().unwrap().is_empty(),
        "a notification is not a question"
    );
    assert_eq!(
        rig.store.session_operator_questions("s1").unwrap().len(),
        0,
        "and it does not show on the session as one either"
    );
    assert_eq!(rig.state(), "running", "a notification is not a pause");
}

/// The regression that keeps the separation meaningful: asking really does
/// create a question, block the caller, and show up in the operator's queue.
#[tokio::test]
async fn asking_still_creates_a_question_that_waits_for_an_answer() {
    state::isolate();
    let rig = rig();
    let tools = rig.tools.clone();
    let ctx = rig.ctx.clone();
    let asking = tokio::spawn(async move {
        tools
            .call(
                &ctx,
                "ask_operator",
                &json!({ "request_id": "q-1", "question": "which remote should this push to?" }),
            )
            .await
    });

    let mut open = Vec::new();
    for _ in 0..200 {
        open = rig.store.open_operator_questions().unwrap();
        if !open.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(open.len(), 1, "asking queues a question");
    assert_eq!(open[0].prompt, "which remote should this push to?");
    assert!(!asking.is_finished(), "and the agent waits for the answer");

    rig.store
        .answer_operator_question(&open[0].id, "\"origin\"")
        .unwrap()
        .unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(5), asking)
        .await
        .expect("the answer releases the caller")
        .unwrap()
        .unwrap();
    assert_eq!(answered["answer"], "origin");
    assert!(rig.store.open_operator_questions().unwrap().is_empty());
}
