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
    /// The operator's API, where a draft is published or discarded.
    operator: axum::Router,
}

fn rig() -> Rig {
    rig_with(Config::default(), Broker::default().shared())
}

fn rig_with(cfg: Config, broker: tracon::broker::SharedBroker) -> Rig {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    store.channel_put("personal", &[], "{}").unwrap();
    store
        .insert_session(&support::rows::session_row("s1", "n1", "personal"))
        .unwrap();
    let cfg = Arc::new(cfg);
    let tools = Arc::new(Tools {
        broker,
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools.session.set(SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    let operator = tracon::http::router(tracon::http::api::AppState {
        manager,
        cfg,
        adapter: Arc::new(support::fake::FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools: tools.clone(),
        mesh: None,
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    });
    Rig {
        store,
        tools,
        ctx: CallContext::session("s1", "personal", "n1"),
        operator,
    }
}

/// A rig whose node can publish to GitHub through a stub `gh` that prints
/// the URL of the issue it pretends to open.
fn publishing_rig(name: &str) -> Rig {
    let dir = state::scratch(&format!("operator-{name}"));
    let gh = dir.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\necho https://github.test/acme/tracon/issues/362\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cfg = Config::default();
    cfg.publish.gh = gh.to_string_lossy().into_owned();
    let broker = toml::from_str(
        r#"
        [credentials.gh]
        channels = ["personal"]
        [credentials.gh.env]
        GH_TOKEN = "brokered"
    "#,
    )
    .unwrap();
    rig_with(cfg, Arc::new(broker))
}

impl Rig {
    async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        self.tools.call(&self.ctx, name, &args).await
    }
    async fn draft(&self, ctx: &CallContext, title: &str) -> String {
        let out = self
            .tools
            .call(
                ctx,
                "report_issue",
                &json!({ "title": title, "expected": "e", "actual": "a" }),
            )
            .await
            .unwrap();
        out["issue_id"].as_str().unwrap().to_string()
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

/// The agent that drafted an issue learns it was published, and as which
/// issue, from the node rather than by searching GitHub for its title.
#[tokio::test]
async fn a_published_draft_reports_its_issue_number_to_the_agent() {
    state::isolate();
    let rig = publishing_rig("published");
    let out = rig
        .call(
            "report_issue",
            json!({ "title": "drafts go quiet", "expected": "e", "actual": "a" }),
        )
        .await
        .unwrap();
    assert!(out["next"]
        .as_str()
        .unwrap()
        .contains("issue_report_status"));
    let id = out["issue_id"].as_str().unwrap().to_string();

    let pending = rig
        .call(
            "issue_report_status",
            json!({ "issue_id": id, "wait_secs": 0 }),
        )
        .await
        .unwrap();
    assert_eq!(pending["state"], "draft", "{pending}");
    assert_eq!(pending["still_waiting"], true);

    let (status, published) = support::http::call(
        &rig.operator,
        "POST",
        &format!("/api/operator/issues/{id}/publish"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{published}");

    let done = rig
        .call("issue_report_status", json!({ "issue_id": id }))
        .await
        .unwrap();
    assert_eq!(done["state"], "published", "{done}");
    assert_eq!(done["number"], 362);
    assert_eq!(done["url"], "https://github.test/acme/tracon/issues/362");
    assert_eq!(done["title"], "drafts go quiet");
    assert_eq!(done["still_waiting"], false);
}

/// Discarding is a decision the agent hears about, with the operator's
/// reason, and it ends the draft: nothing can publish it afterwards. A wait
/// already in progress returns as soon as it is made.
#[tokio::test]
async fn a_discarded_draft_tells_the_waiting_agent_why() {
    state::isolate();
    let rig = publishing_rig("discarded");
    let id = rig.draft(&rig.ctx, "a duplicate of something").await;

    let tools = rig.tools.clone();
    let ctx = rig.ctx.clone();
    let wait_id = id.clone();
    let waiting = tokio::spawn(async move {
        tools
            .call(&ctx, "issue_report_status", &json!({ "issue_id": wait_id }))
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "an undecided draft is waited on");

    let (status, body) = support::http::call(
        &rig.operator,
        "POST",
        &format!("/api/operator/issues/{id}/discard"),
        Some(json!({ "reason": "  already filed as #300  " })),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let out = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the discard releases the wait")
        .unwrap()
        .unwrap();
    assert_eq!(out["state"], "discarded", "{out}");
    assert_eq!(out["reason"], "already filed as #300");

    let (status, _) = support::http::call(
        &rig.operator,
        "POST",
        &format!("/api/operator/issues/{id}/publish"),
        None,
    )
    .await;
    assert_eq!(status, 409, "a discarded draft is not published");
    let (status, _) = support::http::call(
        &rig.operator,
        "POST",
        &format!("/api/operator/issues/{id}/discard"),
        None,
    )
    .await;
    assert_eq!(status, 409, "and is not discarded twice");
    let (status, _) = support::http::call(
        &rig.operator,
        "POST",
        "/api/operator/issues/no-such-draft/discard",
        None,
    )
    .await;
    assert_eq!(status, 404);
}

/// The wait is bounded and says so, for one draft or several.
#[tokio::test]
async fn an_undecided_draft_is_still_waiting_after_a_short_wait() {
    state::isolate();
    let rig = rig();
    let first = rig.draft(&rig.ctx, "one").await;
    let second = rig.draft(&rig.ctx, "two").await;

    let started = std::time::Instant::now();
    let out = rig
        .call(
            "issue_report_status",
            json!({ "issue_id": first, "wait_secs": 1 }),
        )
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(out["state"], "draft");
    assert_eq!(out["still_waiting"], true);

    let both = rig
        .call(
            "issue_report_status",
            json!({ "issue_ids": [first, second], "wait_secs": 0 }),
        )
        .await
        .unwrap();
    assert_eq!(both["still_waiting"], true, "{both}");
    assert_eq!(both["issues"].as_array().unwrap().len(), 2);

    assert!(rig
        .call("issue_report_status", json!({}))
        .await
        .unwrap_err()
        .contains("issue_id"));
}

/// A draft is read back only by whoever drafted it: its session, or the
/// channel's external callers on the node that drafted it when there was no
/// session. Another channel's id reads as no draft at all.
#[tokio::test]
async fn another_callers_draft_is_refused() {
    state::isolate();
    let rig = rig();
    let work = CallContext::external(None, "work", "n1");
    let theirs = rig.draft(&work, "on the work channel").await;
    let status = |ctx: CallContext| {
        let tools = rig.tools.clone();
        let theirs = theirs.clone();
        async move {
            tools
                .call(
                    &ctx,
                    "issue_report_status",
                    &json!({ "issue_id": theirs, "wait_secs": 0 }),
                )
                .await
        }
    };

    let refused = status(rig.ctx.clone()).await.unwrap_err();
    assert!(refused.contains("no issue draft"), "{refused}");
    assert!(status(CallContext::external(None, "personal", "n1"))
        .await
        .is_err());
    assert!(
        status(CallContext::external(None, "work", "n2"))
            .await
            .is_err(),
        "the same channel on another node did not draft it"
    );
    let own = status(CallContext::external(Some("lane".into()), "work", "n1"))
        .await
        .unwrap();
    assert_eq!(own["state"], "draft", "{own}");

    let mine = rig.draft(&rig.ctx, "from the session").await;
    assert!(rig
        .tools
        .call(
            &work,
            "issue_report_status",
            &json!({ "issue_id": mine, "wait_secs": 0 }),
        )
        .await
        .is_err());
}
