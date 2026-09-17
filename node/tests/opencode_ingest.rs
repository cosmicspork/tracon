//! What a session remembers about its harness across a disconnection.
//!
//! The adapter tests are about driving OpenCode; these are about the node's
//! own record of what it drove — the durable sequence it resumes from, the
//! identities it maps, and the mutations whose outcome it could not see. Every
//! case here is a thing that only goes wrong when something is interrupted: a
//! stream that drops mid-turn, a node that restarts mid-turn, a permission the
//! harness is still blocked on, a prompt whose answer never came back, a
//! session that is simply gone.
//!
//! They run against the same fake OpenCode server the adapter tests use, so
//! what is asserted is what crossed the wire rather than what the node
//! believes about it.

#[path = "support/mod.rs"]
mod support;
use support::events::{drain_until, DEADLINE};
use support::fake_opencode::{start, wait_for_reply, Fake, HttpApi, PERMISSION, SESSION};
use support::state;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use tracon::adapter::{
    opencode::OpenCodeAdapter, DurableCursor, HarnessAdapter, HarnessEvent, LaunchSpec,
    PermissionReply,
};
use tracon::gateway::native_events::NativeEvents;
use tracon::runner::Runner;
use tracon::session::ingest::{Ingest, Reconcile};
use tracon::session::state::SessionState;
use tracon::session::supervisor::{Command, Supervisor};
use tracon::store::{now_ms, NodeRow, SessionPatch, Store};
use tracon::stream::Bus;

const SESSION_ID: &str = "s1";

fn store_with_session(state: &str) -> Arc<Store> {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "t".into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: "opencode".into(),
            harness_pinned: "1.18.30".into(),
            harness_found: Some("1.18.30".into()),
            models_json: None,
            checked_at_ms: Some(now_ms()),
            is_self: 1,
            x25519_pub: None,
            last_seen_ms: None,
            reachable: 1,
            providers_json: None,
            app_version: None,
            wire_contract: None,
            policy_identity: None,
            policy_sha256: None,
            policy_receipt_v1: None,
        })
        .unwrap();
    let mut row = support::rows::session_row(SESSION_ID, "n1", "personal");
    row.harness_id = "opencode".into();
    row.harness_version = "1.18.30".into();
    row.state = state.into();
    store.insert_session(&row).unwrap();
    store
}

/// An ingestion layer over a store, with the supervisor's command channel in
/// hand so a test can see what reconciliation puts back on the queue.
fn ingest_for(store: &Arc<Store>) -> (Arc<Ingest>, mpsc::Receiver<Command>) {
    let (ingest, rx, _) = ingest_and_channel(store);
    (ingest, rx)
}

/// The same, with the native UI's live channel in hand: the tap the adapter's
/// pumps write to through `DurableCursor::observe`.
fn ingest_and_channel(
    store: &Arc<Store>,
) -> (Arc<Ingest>, mpsc::Receiver<Command>, Arc<NativeEvents>) {
    let (tx, rx) = mpsc::channel(16);
    let native = NativeEvents::new();
    let ingest = Ingest::new(
        store.clone(),
        Bus::new(),
        "n1".into(),
        SESSION_ID.into(),
        Instant::now(),
        tx,
        native.clone(),
    );
    (ingest, rx, native)
}

fn kinds(store: &Store) -> Vec<String> {
    store
        .events_after(SESSION_ID, 0, 1000)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

fn launch_spec(cursor: Arc<dyn DurableCursor>) -> LaunchSpec {
    LaunchSpec {
        cursor: Some(cursor),
        ..support::fake_opencode::spec()
    }
}

async fn await_true(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened before the deadline");
}

/// A pinned OpenCode v2 read ask, through the adapter's server-wide stream and
/// the real supervisor policy path. The harness stays all-ask; the node is the
/// component that exempts this one request and answers it `once`.
#[tokio::test]
async fn an_opencode_read_is_normalized_and_allowed_by_the_supervisor() {
    state::isolate();
    let store = store_with_session("starting");
    let (tx, cmd_rx) = mpsc::channel(16);
    let ingest = Ingest::new(
        store.clone(),
        Bus::new(),
        "n1".into(),
        SESSION_ID.into(),
        Instant::now(),
        tx.clone(),
        NativeEvents::new(),
    );
    let (runner, seen) =
        start(Fake::new("1.18.30", usize::MAX).permission("read", &["/work/src/lib.rs"])).await;
    let (handle, events) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, launch_spec(ingest.clone()))
        .await
        .unwrap();
    let supervisor = Supervisor::new(
        SESSION_ID.into(),
        "n1".into(),
        store.clone(),
        Bus::new(),
        Arc::from(handle),
        Instant::now(),
        Duration::from_secs(60),
        tx.clone(),
        Arc::new(NoRunner),
        "tracon-h-test".into(),
        tracon::policy::Policy::shipped_shared(),
        "personal".into(),
    )
    .with_ingest(ingest);
    tokio::spawn(supervisor.run(events, cmd_rx));
    let started = store.clone();
    await_true("the session starts", move || {
        started
            .get_session(SESSION_ID)
            .unwrap()
            .is_some_and(|session| session.state == "running")
    })
    .await;

    let (ack, admitted) = oneshot::channel();
    tx.send(Command::Prompt {
        text: "read the file".into(),
        ack,
    })
    .await
    .unwrap();
    admitted.await.unwrap().unwrap();

    let replies = wait_for_reply(&seen).await;
    assert_eq!(replies[0]["body"]["reply"], "once", "{replies:?}");
    assert_ne!(replies[0]["body"]["reply"], "always");
    assert!(
        store.open_permissions().unwrap().is_empty(),
        "a shipped read exemption interrupted the operator"
    );
    let allowed = store
        .events_after(SESSION_ID, 0, 500)
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "policy_allowed")
        .expect("the node records its automatic decision");
    assert_eq!(allowed.payload["action"], "read");
    assert_eq!(allowed.payload["kind"], "read");
    assert_eq!(allowed.payload["resource"], "/work/src/lib.rs");
    assert_eq!(allowed.payload["command"], serde_json::Value::Null);
    assert_eq!(allowed.payload["rule"], "reads-and-thoughts");
    assert!(
        !store
            .events_after(SESSION_ID, 0, 500)
            .unwrap()
            .iter()
            .any(|event| event.kind == "permission_request"),
        "the read took a second path around the supervisor"
    );
    let _ = tx.send(Command::Kill).await;
}

/// A server that re-delivers everything on every connection — which the real
/// one will do whenever the node asks for a sequence it has already ingested,
/// and which a reconnect's overlap produces anyway. The node has to recognise
/// the second delivery and drop it, or a dropped connection duplicates a turn
/// in the transcript.
#[tokio::test]
async fn a_sequence_delivered_twice_produces_one_event() {
    state::isolate();
    let store = store_with_session("running");
    let (ingest, _commands) = ingest_for(&store);
    // Cut mid-turn, then replay from the start: the reconnect carries every
    // sequence the first connection already delivered.
    let fake = Fake::new("1.18.30", 2).replaying_everything();
    let (runner, _seen) = start(fake).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, launch_spec(ingest.clone()))
        .await
        .expect("the harness starts");

    let turn = tokio::spawn(async move { handle.prompt("fix the validation".into()).await });
    let mut labels = Vec::new();
    // The permission arrives on the server-wide stream; answering it lets the
    // scripted turn finish.
    let permission = support::events::next_permission(&mut rx, &mut labels).await;
    let HarnessEvent::Permission { reply, .. } = permission else {
        panic!("expected a permission request")
    };
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();
    let result = turn.await.unwrap().expect("the turn completes");
    assert_eq!(result.stop_reason, "end_turn");
    drain_until(&mut rx, &mut labels, "usage").await;

    assert_eq!(
        labels
            .iter()
            .filter(|l| *l == "chunk:working on it")
            .count(),
        1,
        "a re-delivered sequence was translated twice: {labels:?}"
    );
    assert_eq!(
        labels.iter().filter(|l| *l == "tool_call:bash").count(),
        1,
        "{labels:?}"
    );
    // And the node's own record says how far it got, not the adapter's memory.
    assert_eq!(store.opencode_last_seq(SESSION_ID).unwrap(), 5);
    assert_eq!(
        store
            .opencode_session_of(SESSION_ID)
            .unwrap()
            .unwrap()
            .upstream_id,
        SESSION
    );
}

/// The number `?after=` resumes from is the node's, in its store. A process
/// that dies mid-turn and comes back must ask for what it has not ingested —
/// not from zero, which would replay the turn, and not from nothing, which
/// would lose it.
#[tokio::test]
async fn a_restart_mid_turn_resumes_from_the_persisted_sequence() {
    state::isolate();
    let store = store_with_session("running");
    // A previous process got as far as the text of the turn and then died.
    {
        let (before, _commands) = ingest_for(&store);
        before.rebind(
            SESSION,
            HttpApi::connect("127.0.0.1:1".parse().unwrap(), ""),
        );
        for seq in 1..=2 {
            assert!(
                before
                    .admit(&support::fake_opencode::durable(
                        seq,
                        "session.next.text.ended",
                        json!({ "sessionID": SESSION, "text": "working on it" }),
                    ))
                    .await
            );
        }
    }
    assert_eq!(store.opencode_last_seq(SESSION_ID).unwrap(), 2);

    // A new process, a new adapter, the same store.
    let (ingest, _commands) = ingest_for(&store);
    let (runner, seen) = start(Fake::new("1.18.30", usize::MAX)).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, launch_spec(ingest.clone()))
        .await
        .unwrap();
    let turn = tokio::spawn(async move { handle.prompt("fix the validation".into()).await });
    let mut labels = Vec::new();
    let HarnessEvent::Permission { reply, .. } =
        support::events::next_permission(&mut rx, &mut labels).await
    else {
        panic!("expected a permission request")
    };
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();
    turn.await.unwrap().expect("the turn completes");
    drain_until(&mut rx, &mut labels, "usage").await;

    assert_eq!(
        seen.lock().unwrap().resumed_from.first().copied(),
        Some(2),
        "the restarted node asked from where the store said, not from zero"
    );
    assert!(
        !labels.contains(&"chunk:working on it".to_string()),
        "a sequence the dead process had already ingested was ingested again: {labels:?}"
    );
    assert!(labels.contains(&"tool_call:bash".to_string()), "{labels:?}");
    assert_eq!(store.opencode_last_seq(SESSION_ID).unwrap(), 5);
}

/// The server-wide stream that carries permission asks has no replay at all,
/// so a request raised while the node was disconnected is invisible until
/// somebody asks. Reconciliation asks, and puts it back on the queue — through
/// the supervisor's own command channel, so it gets the policy, the card and
/// the expiry every other request gets.
#[tokio::test]
async fn a_permission_pending_upstream_is_reraised_after_a_reconnect() {
    state::isolate();
    let store = store_with_session("running");
    let (ingest, mut commands) = ingest_for(&store);
    let fake = Fake::new("1.18.30", usize::MAX);
    fake.pending_permission(PERMISSION, "call_1");
    let seen = fake.seen.clone();
    let password = fake.password.clone();
    let addr = support::fake_opencode::serve(fake).await;
    ingest.rebind(
        SESSION,
        HttpApi::connect(addr, &password.lock().unwrap().clone()),
    );

    let report = ingest.reconcile(Reconcile::Reconnect).await;
    assert_eq!(report.reraised, 1, "{report:?}");
    assert_eq!(report.resent, 0);

    let Some(Command::Permission { request, reply }) = commands.recv().await else {
        panic!("the request was never put back on the queue")
    };
    assert!(request.title.contains("bash"), "{}", request.title);
    assert_eq!(request.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(request.action, "bash");
    assert_eq!(request.kind.as_deref(), Some("execute"));
    assert_eq!(request.resource, None);
    assert_eq!(request.command.as_deref(), Some("just test"));
    assert_eq!(request.raw_input.unwrap()["reraised"], true);
    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();

    // The answer reaches the harness, and reaches it as `once`.
    let replies = support::fake_opencode::wait_for_reply(&seen).await;
    assert_eq!(replies[0]["id"], PERMISSION);
    assert_eq!(replies[0]["body"]["reply"], "once");

    // It was recorded as the same request, so a second reconciliation does not
    // ask the operator twice.
    let again = ingest.reconcile(Reconcile::Reconnect).await;
    assert_eq!(again.reraised, 0, "{again:?}");
    assert!(kinds(&store).contains(&"permission_request".to_string()));
}

/// A permission the operator already answered, that the harness is somehow
/// still blocked on. Re-sending is safe — the reply route is idempotent — and
/// the alternative is a harness waiting forever on a decision that was made.
#[tokio::test]
async fn a_permission_answered_here_but_pending_upstream_is_answered_again() {
    state::isolate();
    let store = store_with_session("running");
    let (ingest, _commands) = ingest_for(&store);
    let fake = Fake::new("1.18.30", usize::MAX);
    fake.pending_permission(PERMISSION, "call_1");
    let seen = fake.seen.clone();
    let password = fake.password.clone();
    let addr = support::fake_opencode::serve(fake).await;
    ingest.rebind(
        SESSION,
        HttpApi::connect(addr, &password.lock().unwrap().clone()),
    );

    // The node's own record of that request, already answered.
    let row = tracon::store::PermissionRow {
        id: "p1".into(),
        session_id: SESSION_ID.into(),
        node_id: "n1".into(),
        rpc_id: 0,
        tool_call_id: Some("call_1".into()),
        title: "bash: just test".into(),
        kind: Some("tool".into()),
        raw_input: None,
        options: "[]".into(),
        state: "answered".into(),
        answer_option_id: Some("allow_once".into()),
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: Some(1),
        expires_ms: now_ms() + 60_000,
    };
    store.insert_permission(&row).unwrap();

    let report = ingest.reconcile(Reconcile::Reconnect).await;
    assert_eq!(report.resent, 1, "{report:?}");
    assert_eq!(
        report.reraised, 0,
        "the operator was not asked a second time"
    );
    let replies = seen.lock().unwrap().replies.clone();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["body"]["reply"], "once");
}

/// The prompt is the one mediated mutation that must never be re-sent on a
/// guess: a second one duplicates a turn's work. So a dispatch that does not
/// report leaves the session `uncertain`, the next prompt is refused with the
/// reason, and reconciliation asks the harness what it actually holds.
#[tokio::test]
async fn a_prompt_that_never_reported_leaves_the_session_uncertain_until_asked() {
    state::isolate();
    let store = store_with_session("starting");
    let (tx, cmd_rx) = mpsc::channel(16);
    let ingest = Ingest::new(
        store.clone(),
        Bus::new(),
        "n1".into(),
        SESSION_ID.into(),
        Instant::now(),
        tx.clone(),
        NativeEvents::new(),
    );
    // Nothing here is about the queue: the scripted permission would move the
    // session to `waiting_on_you` and refuse the second prompt for a reason
    // this test is not asking about.
    let fake = Fake::new("1.18.30", usize::MAX).never_asks();
    let peek = fake.clone();
    let (runner, _seen) = start(fake).await;
    let (handle, events) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, launch_spec(ingest.clone()))
        .await
        .unwrap();
    let supervisor = Supervisor::new(
        SESSION_ID.into(),
        "n1".into(),
        store.clone(),
        Bus::new(),
        Arc::from(handle),
        Instant::now(),
        Duration::from_secs(60),
        tx.clone(),
        Arc::new(NoRunner),
        "tracon-h-test".into(),
        Default::default(),
        "personal".into(),
    )
    .with_ingest(ingest.clone());
    tokio::spawn(supervisor.run(events, cmd_rx));
    let state_store = store.clone();
    await_true("the session starts", move || {
        state_store
            .get_session(SESSION_ID)
            .unwrap()
            .is_some_and(|s| s.state == "running")
    })
    .await;

    // The harness admits the prompt and then fails to report on it.
    peek.prompt_times_out();
    let (ack, wait) = oneshot::channel();
    tx.send(Command::Prompt {
        text: "fix the validation".into(),
        ack,
    })
    .await
    .unwrap();
    wait.await.unwrap().expect("the dispatch is accepted");

    let uncertain = ingest.clone();
    await_true("the session becomes uncertain", move || {
        uncertain.is_uncertain()
    })
    .await;
    assert!(kinds(&store).contains(&"uncertain".to_string()));

    // A second prompt is refused rather than sent on a guess.
    let (ack, wait) = oneshot::channel();
    tx.send(Command::Prompt {
        text: "again".into(),
        ack,
    })
    .await
    .unwrap();
    let refused = wait.await.unwrap().expect_err("a prompt must be refused");
    assert!(refused.contains("unknown"), "{refused}");
    assert_eq!(
        peek.prompt_count(),
        1,
        "the refused prompt reached the harness anyway"
    );

    // Asking settles it: the harness holds the prompt, so it was admitted and
    // is not sent again.
    let report = ingest.reconcile(Reconcile::Reconnect).await;
    assert_eq!(report.settled, 1, "{report:?}");
    assert!(!ingest.is_uncertain());
    assert_eq!(
        peek.prompt_count(),
        1,
        "reconciliation re-sent a prompt instead of asking about it"
    );

    // And the session takes prompts again.
    peek.prompt_answers();
    let (ack, wait) = oneshot::channel();
    tx.send(Command::Prompt {
        text: "carry on".into(),
        ack,
    })
    .await
    .unwrap();
    wait.await.unwrap().expect("the refusal lifted");
    let sent = peek.clone();
    await_true("the prompt reaches the harness", move || {
        sent.prompt_count() == 2
    })
    .await;
}

/// A session OpenCode made for itself. There is no route to create one through
/// tracon, so it is recorded with its lineage and shown to the operator as
/// unregistered — never adopted as a worker the node cannot supervise.
#[tokio::test]
async fn a_child_session_is_recorded_with_its_lineage_and_surfaced_as_untracked() {
    state::isolate();
    let store = store_with_session("running");
    let (ingest, _commands) = ingest_for(&store);
    ingest.rebind(
        SESSION,
        HttpApi::connect("127.0.0.1:1".parse().unwrap(), ""),
    );

    ingest
        .admit(&support::fake_opencode::durable(
            1,
            "session.created",
            json!({
                "sessionID": "ses_child0000000000000000",
                "info": { "id": "ses_child0000000000000000", "parentID": SESSION },
            }),
        ))
        .await;

    let children = store.opencode_children(SESSION).unwrap();
    assert_eq!(children.len(), 1);
    assert!(children[0].is_untracked());
    assert_eq!(children[0].session_id, None, "it is not a tracon session");
    assert_eq!(children[0].upstream_id, "ses_child0000000000000000");

    let event = store
        .events_after(SESSION_ID, 0, 100)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "child_session")
        .expect("the operator is told about a session the node did not create");
    assert_eq!(event.payload["untracked"], true);
    assert_eq!(event.payload["parent"], SESSION);

    // Told once, not on every replay of the same event.
    ingest
        .admit(&support::fake_opencode::durable(
            1,
            "session.created",
            json!({
                "sessionID": "ses_child0000000000000000",
                "info": { "id": "ses_child0000000000000000", "parentID": SESSION },
            }),
        ))
        .await;
    assert_eq!(
        kinds(&store)
            .iter()
            .filter(|k| *k == "child_session")
            .count(),
        1
    );
}

/// A session the harness no longer has is not a session that is quietly idle.
/// It ends, visibly, with the reason on the row.
#[tokio::test]
async fn a_session_gone_upstream_becomes_terminal_with_the_reason() {
    state::isolate();
    let store = store_with_session("running");
    let (ingest, _commands) = ingest_for(&store);
    let fake = Fake::new("1.18.30", usize::MAX);
    let peek = fake.clone();
    let password = fake.password.clone();
    let addr = support::fake_opencode::serve(fake).await;
    ingest.rebind(
        SESSION,
        HttpApi::connect(addr, &password.lock().unwrap().clone()),
    );

    // While it is there, nothing happens to the session.
    let alive = ingest.reconcile(Reconcile::Reconnect).await;
    assert!(!alive.gone);
    assert_eq!(
        store.get_session(SESSION_ID).unwrap().unwrap().state,
        "running"
    );

    peek.gone();
    let report = ingest.reconcile(Reconcile::Reconnect).await;
    assert!(report.gone, "{report:?}");
    let row = store.get_session(SESSION_ID).unwrap().unwrap();
    assert!(
        SessionState::from_stored(&row.state).is_terminal(),
        "{row:?}"
    );
    assert_eq!(row.end_reason.as_deref(), Some("harness_exit"));
    assert!(
        row.last_error
            .unwrap()
            .contains("no longer has this session"),
        "the operator is owed the reason, not just the state"
    );
    assert!(store.opencode_session_of(SESSION_ID).unwrap().unwrap().gone != 0);
}

/// Startup, with no stream to replay through: the durable history is read and
/// ingested, the turn that ended while the node was away is closed with its
/// usage, and doing it twice changes nothing — the sequence is what makes it
/// safe to run again.
#[tokio::test]
async fn a_startup_reconciliation_ingests_the_missed_turn_exactly_once() {
    state::isolate();
    let store = store_with_session("running");
    store
        .update_session(
            SESSION_ID,
            SessionPatch {
                turn_active: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let (ingest, _commands) = ingest_for(&store);
    let fake = Fake::new("1.18.30", usize::MAX);
    let password = fake.password.clone();
    let addr = support::fake_opencode::serve(fake).await;
    ingest.rebind(
        SESSION,
        HttpApi::connect(addr, &password.lock().unwrap().clone()),
    );

    let report = ingest.reconcile(Reconcile::Startup).await;
    assert_eq!(report.ingested, 5, "{report:?}");
    assert!(report.turn_closed, "{report:?}");
    let after_first = kinds(&store);
    assert_eq!(after_first.iter().filter(|k| *k == "message").count(), 1);
    assert_eq!(after_first.iter().filter(|k| *k == "tool_call").count(), 1);
    assert_eq!(
        after_first.iter().filter(|k| *k == "tool_result").count(),
        1
    );
    assert_eq!(after_first.iter().filter(|k| *k == "turn_end").count(), 1);

    let row = store.get_session(SESSION_ID).unwrap().unwrap();
    assert_eq!(row.turn_active, 0, "the stale active turn was closed");
    assert_eq!(row.tokens_used, 1034, "closed with the usage it reported");

    // Again, from a store that already holds the sequence.
    let again = ingest.reconcile(Reconcile::Startup).await;
    assert_eq!(again.ingested, 0, "{again:?}");
    assert!(!again.turn_closed);
    assert_eq!(kinds(&store).len(), after_first.len());
    assert_eq!(store.opencode_last_seq(SESSION_ID).unwrap(), 5);
}

/// The supervisor tears down its container on exit; these tests have none.
struct NoRunner;

#[async_trait::async_trait]
impl Runner for NoRunner {
    async fn spawn(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        unreachable!("the harness is already running")
    }
    async fn run_capture(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        unreachable!("the harness is already running")
    }
    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

/// The tap the native UI's live channel is built from is fed by the pumps the
/// node already runs, and by nothing else.
///
/// Finding 20 is a seam between two true things: the app streams only from
/// `GET /global/event`, and that stream is unforwardable. The node closes it by
/// serving its own — but only if the events actually arrive, and only if they
/// arrive without a second reader. So this drives the real adapter against the
/// fake server, with the real `Ingest` as the cursor, and asserts the ask
/// reaches the channel in the shape the page reads.
///
/// The permission here is the same one the rest of this file uses: raised on
/// the server-wide stream, because the durable per-session one does not carry
/// asks at all (finding 6). That is exactly why the tap is on both pumps.
#[tokio::test]
async fn the_native_channel_is_fed_from_the_pumps_the_node_already_runs() {
    state::isolate();
    let store = store_with_session("running");
    let (ingest, _commands, native) = ingest_and_channel(&store);
    let fake = Fake::new("1.18.30", usize::MAX);
    let (runner, seen) = start(fake).await;
    let (handle, mut rx) = OpenCodeAdapter::new("1.18.30")
        .launch(&runner, launch_spec(ingest.clone()))
        .await
        .expect("the harness starts");

    let turn = tokio::spawn(async move { handle.prompt("fix the validation".into()).await });
    let mut labels = Vec::new();
    let permission = support::events::next_permission(&mut rx, &mut labels).await;
    let HarnessEvent::Permission { reply, .. } = permission else {
        panic!("expected a permission request")
    };

    // The ask is on the page's channel, in the v1 shape the app switches on —
    // not the `permission.v2.asked` the server put on the wire.
    await_true("the ask reached the native channel", || {
        native
            .replay(0)
            .iter()
            .any(|f| f.payload["type"] == "permission.asked")
    })
    .await;
    let frame = native
        .replay(0)
        .into_iter()
        .find(|f| f.payload["type"] == "permission.asked")
        .expect("the ask");
    assert_eq!(frame.payload["properties"]["id"], PERMISSION);
    assert_eq!(frame.payload["properties"]["sessionID"], SESSION);
    assert_eq!(frame.payload["properties"]["permission"], "bash");
    assert_eq!(frame.payload["properties"]["tool"]["callID"], "call_1");
    // The node's own sequence, which is what makes the stream resumable at all.
    assert!(frame.id > 0);

    reply
        .send(PermissionReply::Selected("allow_once".into()))
        .unwrap();
    turn.await.unwrap().expect("the turn completes");

    // One reader per upstream stream, still. The channel opened nothing of its
    // own: if it had, the durable stream would have been connected more than
    // once and the sequence it exists to protect would be raced — which is the
    // bug the sequence was introduced to prevent (finding 6).
    let resumed = seen.lock().unwrap().resumed_from.len();
    assert_eq!(
        resumed, 1,
        "the durable stream was opened {resumed} times; the native channel must tee, not re-read"
    );
}
