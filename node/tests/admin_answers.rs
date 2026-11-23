//! Answers that are admin actions. Allowing a repository setup proposal
//! writes `node.toml`; allowing a Dockerfile trial builds it with the network,
//! outside the boundary; allowing an egress ask for the repository saves the
//! host for every later session. Away from the node each needs a passkey
//! bound to that answer; a peer's answer needs the sender in `[mesh]
//! admin_peers` and a statement its operator confirmed for exactly that
//! answer; and every other answer is as it was.

#[path = "support/mod.rs"]
mod support;
use support::{
    harness::{harness_as, harness_with, Harness},
    soft_passkey::SoftPasskey,
};

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use proto::admin::AdminStatement;
use proto::frame::{Command, MESH_CHANNEL};
use proto::keys::Identity;
use tracon::{
    config::Config,
    http::{api::AppState, auth, passkey::STEP_UP_HEADER},
    mesh::forward::CommandExecutor,
    store::{approvals::ApprovalRow, now_ms, AuthSessionRow},
};

const ORIGIN: &str = "https://tracon.example.org";
const REMOTE: &str = "203.0.113.7:5000";
const LOCAL: &str = "127.0.0.1:5000";
const COOKIE: &str = "tracon_session=phone";

async fn send(
    h: &Harness,
    method: &str,
    uri: &str,
    peer: &str,
    body: &Value,
    proof: Option<&str>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "host",
            if peer == LOCAL {
                "127.0.0.1:7420"
            } else {
                "tracon.example.org"
            },
        )
        .header("content-type", "application/json");
    if peer != LOCAL {
        b = b.header("cookie", COOKIE).header("origin", ORIGIN);
    }
    if let Some(p) = proof {
        b = b.header(STEP_UP_HEADER, p);
    }
    let mut req = b.body(Body::from(body.to_string())).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let res = h.operator.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A node with a public origin, a phone paired a moment ago and its passkey.
async fn rig() -> (Harness, SoftPasskey) {
    let mut cfg = Config::default();
    cfg.auth.public_origin = Some(ORIGIN.into());
    let h = harness_with(cfg).await;
    let now = now_ms();
    h.store
        .auth_session_insert(&AuthSessionRow {
            token_hash: auth::hash("phone"),
            created_ms: now,
            last_seen_ms: now,
            expires_ms: now + 600_000,
            user_agent: None,
            paired_ms: Some(now),
            delegated_from: None,
        })
        .unwrap();
    let mut key = SoftPasskey::new(1, ORIGIN);
    let (s, opts) = send(
        &h,
        "POST",
        "/api/passkeys/register/options",
        REMOTE,
        &json!({}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{opts}");
    let credential = key.create(&opts["options"]);
    let (s, v) = send(
        &h,
        "POST",
        "/api/passkeys/register/finish",
        REMOTE,
        &json!({ "challenge_id": opts["challenge_id"], "credential": credential }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    (h, key)
}

fn approval(h: &Harness, id: &str, tool: &str, node: &str, arguments: Value) {
    let now = now_ms();
    h.store
        .insert_approval(&ApprovalRow {
            id: id.into(),
            channel: "personal".into(),
            session_id: None,
            node_id: node.into(),
            lane: None,
            tool: tool.into(),
            arguments: arguments.to_string(),
            request_key: format!("key-{id}"),
            title: tool.into(),
            state: "pending".into(),
            answer_option_id: None,
            edited_arguments: None,
            result: None,
            reason: None,
            operator_note: None,
            claimed_ms: None,
            created_ms: now,
            decided_ms: None,
            finished_ms: None,
            expires_ms: now + 600_000,
        })
        .unwrap();
}

fn state_of(h: &Harness, id: &str) -> String {
    h.store.get_approval(id).unwrap().unwrap().state
}

/// The admin answers: which approval, and the option that allows it.
fn admin_answers(h: &Harness) -> Vec<(&'static str, &'static str)> {
    admin_answers_on(h, "n1")
}

/// As [`admin_answers`], held by `node`.
fn admin_answers_on(h: &Harness, node: &str) -> Vec<(&'static str, &'static str)> {
    approval(
        h,
        "propose",
        "repo_setup_propose",
        node,
        json!({ "checks": ["just check"], "repo": "/work/x" }),
    );
    approval(
        h,
        "trial",
        "repo_setup_try",
        node,
        json!({ "dockerfile": "Dockerfile", "repo": "/work/x" }),
    );
    approval(
        h,
        "egress",
        "request_egress",
        node,
        json!({ "host": "example.org" }),
    );
    approval(
        h,
        "plugin",
        "plugin_install",
        node,
        json!({ "name": "sql-review", "version": "0.1.0", "repo": "cosmicspork/sql-review" }),
    );
    admin_options()
}

/// Each admin card [`admin_answers_on`] makes, and the option that allows it.
fn admin_options() -> Vec<(&'static str, &'static str)> {
    vec![
        ("propose", "allow_once"),
        ("trial", "allow_once"),
        ("egress", "allow_repo"),
        ("plugin", "allow_once"),
    ]
}

#[tokio::test]
async fn an_admin_answer_from_afar_needs_a_passkey_bound_to_it() {
    let (h, mut key) = rig().await;
    for (id, option) in admin_answers(&h) {
        let uri = format!("/api/permissions/{id}/answer");
        let body = json!({ "option_id": option });
        let (s, refused) = send(&h, "POST", &uri, REMOTE, &body, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{id}: {refused}");
        assert_eq!(
            refused["error"]["kind"], "step_up_required",
            "{id}: {refused}"
        );
        assert_eq!(state_of(&h, id), "pending", "{id}");
        // The prompt says what is being confirmed, in the node's words from
        // the stored approval.
        let summary = refused["step_up"]["summary"].as_str().unwrap_or_default();
        let expected = match id {
            "propose" => "Allow repository setup for /work/x",
            "trial" => "Build Dockerfile for /work/x with the network",
            "plugin" => "Install plugin sql-review 0.1.0 from cosmicspork/sql-review",
            _ => "Save example.org to the repository's egress",
        };
        assert_eq!(summary, expected, "{id}: {refused}");

        let step_up = &refused["step_up"];
        let credential = key.get(&step_up["options"]);
        let (s, finished) = send(
            &h,
            "POST",
            "/api/passkeys/assert/finish",
            REMOTE,
            &json!({ "challenge_id": step_up["challenge_id"], "credential": credential }),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{id}: {finished}");
        let proof = finished["proof"].as_str().unwrap().to_string();

        // A proof for this answer does not allow it with other arguments,
        // and the prompt for those says which were edited.
        let edited = json!({ "option_id": option, "arguments": { "checks": ["curl evil"] } });
        let (s, v) = send(&h, "POST", &uri, REMOTE, &edited, Some(&proof)).await;
        assert_eq!(v["error"]["kind"], "step_up_required", "{id}: {s} {v}");
        assert_eq!(
            v["step_up"]["summary"].as_str().unwrap_or_default(),
            format!("Edited checks: {expected}")
        );

        // The answer it was made for goes past the gate.
        let (s, v) = send(&h, "POST", &uri, REMOTE, &body, Some(&proof)).await;
        assert_ne!(v["error"]["kind"], "step_up_required", "{id}: {s} {v}");
        assert_ne!(s, StatusCode::FORBIDDEN, "{id}: {v}");
    }
    // The two the node runs itself were decided: no longer waiting.
    assert_ne!(state_of(&h, "propose"), "pending");
    assert_ne!(state_of(&h, "trial"), "pending");
}

/// What an agent wrote is shown cleaned: no bidirectional overrides,
/// zero-width or separator characters to disguise a host, and bounded.
#[tokio::test]
async fn an_agents_words_in_the_prompt_are_cleaned_and_bounded() {
    let (h, _key) = rig().await;
    let host = format!(
        "evil\u{202E}gro.elpmaxe\u{200B}\u{2028}.test{}",
        "x".repeat(400)
    );
    approval(
        &h,
        "sneaky",
        "request_egress",
        "n1",
        json!({ "host": host }),
    );
    let (s, refused) = send(
        &h,
        "POST",
        "/api/permissions/sneaky/answer",
        REMOTE,
        &json!({ "option_id": "allow_repo", "arguments": { "host": "a", "zz": 1, "\u{202E}k": 2 } }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{refused}");
    let summary = refused["step_up"]["summary"].as_str().unwrap();
    // Which arguments were edited, by name, first, so no cap cuts it off.
    assert!(
        summary.starts_with("Edited host, k, zz: Save evilgro.elpmaxe.testxxx"),
        "{summary}"
    );
    assert!(
        !summary
            .chars()
            .any(|c| matches!(c as u32, 0x200B | 0x2028 | 0x202E)),
        "{summary:?}"
    );
    assert!(
        summary.chars().count() <= 160,
        "{}",
        summary.chars().count()
    );
}

#[tokio::test]
async fn at_the_node_an_admin_answer_needs_no_passkey() {
    let (h, _key) = rig().await;
    for (id, option) in admin_answers(&h) {
        let uri = format!("/api/permissions/{id}/answer");
        let (s, v) = send(
            &h,
            "POST",
            &uri,
            LOCAL,
            &json!({ "option_id": option }),
            None,
        )
        .await;
        assert_ne!(s, StatusCode::FORBIDDEN, "{id}: {v}");
    }
    assert_ne!(state_of(&h, "propose"), "pending");
}

#[tokio::test]
async fn refusing_and_ordinary_approvals_need_no_passkey() {
    let (h, _key) = rig().await;
    admin_answers(&h);
    // Refusing an admin approval, or asking for changes, changes nothing.
    let (s, v) = send(
        &h,
        "POST",
        "/api/permissions/propose/answer",
        REMOTE,
        &json!({ "option_id": "reject_once", "reason": "not yet" }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(state_of(&h, "propose"), "rejected");
    // Opening a host for this session alone is the session's own grant.
    let (s, v) = send(
        &h,
        "POST",
        "/api/permissions/egress/answer",
        REMOTE,
        &json!({ "option_id": "allow_session" }),
        None,
    )
    .await;
    assert_ne!(v["error"]["kind"], "step_up_required", "{s} {v}");
    // An ordinary brokered call is answered from the phone as before.
    approval(
        &h,
        "plain",
        "jira_comment",
        "n1",
        json!({ "issue": "X-1", "body": "hi" }),
    );
    let (s, v) = send(
        &h,
        "POST",
        "/api/permissions/plain/answer",
        REMOTE,
        &json!({ "option_id": "allow_once" }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_ne!(state_of(&h, "plain"), "pending");
}

#[tokio::test]
async fn a_non_json_answer_body_is_refused() {
    let (h, _key) = rig().await;
    admin_answers(&h);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/permissions/propose/answer")
        .header("host", "tracon.example.org")
        .header("cookie", COOKIE)
        .header("content-type", "text/plain")
        .body(Body::from(json!({ "option_id": "allow_once" }).to_string()))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(REMOTE.parse::<SocketAddr>().unwrap()));
    let res = h.operator.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(state_of(&h, "propose"), "pending");
}

/// This node's own identity in the mesh tests: a statement names a real
/// node id.
fn me() -> Identity {
    Identity::from_seed(&[40u8; 32])
}

/// A node (`me`) whose `[mesh] admin_peers` names `admins`, with `peer` a
/// member of the mesh and of the approvals' channel, as this node is, and
/// the admin cards pending.
async fn mesh_rig(peer: &Identity, admins: &[String]) -> (Harness, AppState) {
    let mut cfg = Config::default();
    cfg.auth.public_origin = Some(ORIGIN.into());
    cfg.mesh.admin_peers = admins.to_vec();
    let h = harness_as(
        cfg,
        Arc::new(tracon::runner::local::LocalBackend),
        &me().node_id(),
    )
    .await;
    let state = h.state.clone();
    for node in [peer.node_id(), me().node_id()] {
        h.store
            .node_channels_set(&node, &[MESH_CHANNEL.into(), "personal".into()])
            .unwrap();
    }
    admin_answers_on(&h, &me().node_id());
    (h, state)
}

fn answer(id: &str, option: &str, arguments: Option<Value>) -> Command {
    Command::Answer {
        permission_id: id.into(),
        option_id: option.into(),
        arguments,
        reason: None,
        notes: None,
        arguments_hash: None,
        preview_hash: None,
        statement: None,
    }
}

/// What `from`'s operator confirmed for exactly `command` on `target`, at
/// `at`, as `from` sends it.
fn confirmed_for(from: &Identity, target: &str, command: &Command, at: i64) -> AdminStatement {
    AdminStatement::sign(
        from,
        target,
        &tracon::mesh::admin::op_hash(target, command),
        "On n1: allow it",
        "local",
        at,
    )
}

fn with(command: &Command, statement: AdminStatement) -> Command {
    let mut command = command.clone();
    if let Command::Answer { statement: s, .. } = &mut command {
        *s = Some(Box::new(statement));
    }
    command
}

async fn execute(state: &AppState, sender: &Identity, command: Command) -> Result<Value, String> {
    state.execute(&sender.node_id(), command).await
}

/// An admin peer's answer to an admin card, with a statement its operator
/// confirmed for exactly that answer here, is decided, once: the same
/// statement again is refused.
#[tokio::test]
async fn an_admin_peer_answers_an_admin_card_with_a_statement_for_exactly_that_answer() {
    let peer = Identity::from_seed(&[41u8; 32]);
    let (h, state) = mesh_rig(&peer, &[peer.node_id()]).await;
    for (id, option) in admin_options() {
        let bare = answer(id, option, None);
        let signed = with(
            &bare,
            confirmed_for(&peer, &me().node_id(), &bare, now_ms()),
        );
        let result = execute(&state, &peer, signed.clone()).await;
        // Past the gate: whatever the card's own run makes of it here, it
        // is not refused as an admin answer.
        if let Err(e) = &result {
            assert!(
                !e.contains("admin") && !e.contains("statement"),
                "{id}: {e}"
            );
        }
        let replayed = execute(&state, &peer, signed).await.expect_err(id);
        assert!(replayed.contains("already used"), "{id}: {replayed}");
    }
    assert_ne!(state_of(&h, "propose"), "pending");
    assert_ne!(state_of(&h, "trial"), "pending");
}

/// Without `[mesh] admin_peers` naming the sender, a statement changes
/// nothing, and the refusal says which check failed.
#[tokio::test]
async fn an_admin_answer_from_a_peer_not_in_admin_peers_is_refused() {
    let peer = Identity::from_seed(&[42u8; 32]);
    let (h, state) = mesh_rig(&peer, &[]).await;
    for (id, option) in admin_options() {
        let bare = answer(id, option, None);
        let signed = with(
            &bare,
            confirmed_for(&peer, &me().node_id(), &bare, now_ms()),
        );
        let refusal = execute(&state, &peer, signed).await.expect_err(id);
        assert!(refusal.contains("admin_peers"), "{id}: {refusal}");
        assert_eq!(state_of(&h, id), "pending", "{id}");
    }
}

/// A statement that is missing, stale, for another card, option, edit or
/// node, or from another issuer is refused, naming what failed, and the
/// card stays pending.
#[tokio::test]
async fn an_admin_answer_needs_a_fresh_statement_for_exactly_it() {
    let peer = Identity::from_seed(&[43u8; 32]);
    let other = Identity::from_seed(&[44u8; 32]);
    let (h, state) = mesh_rig(&peer, &[peer.node_id(), other.node_id()]).await;
    let now = now_ms();
    let propose = answer("propose", "allow_once", None);
    let trial = answer("trial", "allow_once", None);
    let edited = answer(
        "propose",
        "allow_once",
        Some(json!({ "checks": ["curl evil"], "repo": "/work/x" })),
    );
    let egress_once = answer("egress", "allow_once", None);
    let egress_repo = answer("egress", "allow_repo", None);
    let cases: Vec<(&str, Command)> = vec![
        ("carry the statement", propose.clone()),
        (
            "expired",
            with(
                &propose,
                confirmed_for(&peer, &me().node_id(), &propose, now - 10 * 60_000),
            ),
        ),
        (
            "another operation",
            with(&propose, confirmed_for(&peer, &me().node_id(), &trial, now)),
        ),
        (
            "another operation",
            with(
                &edited,
                confirmed_for(&peer, &me().node_id(), &propose, now),
            ),
        ),
        (
            "another operation",
            with(
                &egress_repo,
                confirmed_for(&peer, &me().node_id(), &egress_once, now),
            ),
        ),
        (
            "another node",
            with(
                &propose,
                confirmed_for(
                    &peer,
                    &Identity::from_seed(&[48u8; 32]).node_id(),
                    &propose,
                    now,
                ),
            ),
        ),
        (
            "another node than",
            with(
                &propose,
                confirmed_for(&other, &me().node_id(), &propose, now),
            ),
        ),
    ];
    for (words, command) in cases {
        let refusal = execute(&state, &peer, command).await.expect_err(words);
        assert!(refusal.contains(words), "{words}: {refusal}");
        assert!(
            refusal.starts_with("allowing a"),
            "says what is refused: {refusal}"
        );
    }
    for id in ["propose", "trial", "egress"] {
        assert_eq!(state_of(&h, id), "pending", "{id}");
    }
}

/// An admin peer outside the card's channel is refused for the channel, and
/// its statement is left unspent: once it is a member, the same answer runs.
#[tokio::test]
async fn an_admin_answer_still_needs_the_cards_channel() {
    let peer = Identity::from_seed(&[45u8; 32]);
    let (h, state) = mesh_rig(&peer, &[peer.node_id()]).await;
    h.store
        .node_channels_set(&peer.node_id(), &[MESH_CHANNEL.into()])
        .unwrap();
    let bare = answer("propose", "allow_once", None);
    let signed = with(
        &bare,
        confirmed_for(&peer, &me().node_id(), &bare, now_ms()),
    );
    let refusal = execute(&state, &peer, signed.clone())
        .await
        .expect_err("not a member");
    assert!(
        refusal.contains("not a member of this approval's channel"),
        "{refusal}"
    );
    assert_eq!(state_of(&h, "propose"), "pending");
    h.store
        .node_channels_set(&peer.node_id(), &[MESH_CHANNEL.into(), "personal".into()])
        .unwrap();
    if let Err(e) = execute(&state, &peer, signed).await {
        assert!(!e.contains("statement"), "{e}");
    }
    assert_ne!(state_of(&h, "propose"), "pending");
}

/// Any other answer is decided by channel membership alone, as before: no
/// admin peer, no statement.
#[tokio::test]
async fn a_peers_ordinary_answer_needs_only_the_channel() {
    let peer = Identity::from_seed(&[46u8; 32]);
    let (h, state) = mesh_rig(&peer, &[]).await;
    approval(&h, "plain", "jira_comment", &me().node_id(), json!({}));
    let result = execute(&state, &peer, answer("plain", "allow_once", None)).await;
    if let Err(e) = &result {
        assert!(!e.contains("admin") && !e.contains("statement"), "{e}");
    }
    // Refusing an admin card is no admin action either.
    execute(&state, &peer, answer("propose", "reject_once", None))
        .await
        .unwrap();
    assert_eq!(state_of(&h, "propose"), "rejected");
    // A non-member is refused for the channel, admin card or not.
    let stranger = Identity::from_seed(&[47u8; 32]);
    let refusal = execute(&state, &stranger, answer("trial", "reject_once", None))
        .await
        .expect_err("not a member");
    assert!(refusal.contains("channel"), "{refusal}");
}

/// A card this node holds for a peer is answered through that peer: from
/// here that needs a hub to reach it, and this node has none.
#[tokio::test]
async fn an_admin_approval_held_by_a_peer_is_answered_there() {
    let (h, _key) = rig().await;
    h.store.ensure_peer_node("n2").unwrap();
    approval(&h, "theirs", "repo_setup_propose", "n2", json!({}));
    let (s, v) = send(
        &h,
        "POST",
        "/api/permissions/theirs/answer",
        LOCAL,
        &json!({ "option_id": "allow_once" }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no hub configured"),
        "{v}"
    );
    assert_eq!(state_of(&h, "theirs"), "pending");
}
