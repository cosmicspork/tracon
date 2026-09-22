//! Memory and documents through the surfaces that use them: the MCP tools a
//! session calls, and the operator API the interface and the CLI call.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness;
use support::http::call_with;
use support::state;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn mcp(app: &axum::Router, sid: &str, token: &str, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{sid}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn tool_call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

fn text(v: &Value) -> String {
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn every_session_is_offered_memory_and_document_tools() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in ["recall", "retain", "doc_read", "doc_search", "doc_write"] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    assert!(!names.contains(&"query"), "no credential, no consulta");
}

#[tokio::test]
async fn retain_then_recall_round_trips_and_a_lesson_waits_for_the_batch() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    // A session with no project identity retains globally.
    let v = mcp(&h.harness, "s1", &token, tool_call(1, "retain", json!({"kind": "fact", "scope": "global", "body": "the test command is just test", "confidence": 0.95}))).await;
    assert_eq!(v["result"]["isError"], false, "{v}");
    assert!(text(&v).contains("active"));
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            2,
            "retain",
            json!({"kind": "lesson", "scope": "global", "body": "flaky tests hide behind retries"}),
        ),
    )
    .await;
    assert!(text(&v).contains("candidate"), "{v}");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            3,
            "retain",
            json!({"kind": "directive", "scope": "global", "body": "no"}),
        ),
    )
    .await;
    assert_eq!(
        v["result"]["isError"], true,
        "directives are the operator's"
    );
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            4,
            "retain",
            json!({"kind": "fact", "scope": "project", "body": "x"}),
        ),
    )
    .await;
    assert_eq!(
        v["result"]["isError"], true,
        "no project identity on this session"
    );

    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(5, "recall", json!({"query": "how do I run the tests?"})),
    )
    .await;
    let hits: Vec<Value> = serde_json::from_str::<Value>(&text(&v)).unwrap()["hits"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the candidate lesson is not context: {hits:?}"
    );
    assert_eq!(hits[0]["kind"], "fact");
    // And the operator sees both, with the lesson held.
    let (_, v) = call_with(
        &h.operator,
        "GET",
        "/api/memories?channel=personal",
        None,
        &[],
    )
    .await;
    let states: Vec<&str> = v["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["state"].as_str().unwrap())
        .collect();
    assert!(states.contains(&"active") && states.contains(&"candidate"));
    // A directive from the operator ranks first.
    let (st, v) = call_with(
        &h.operator,
        "POST",
        "/api/memories",
        Some(json!({"channel": "personal", "body": "run just test before every commit"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(6, "recall", json!({"query": "test"})),
    )
    .await;
    let hits: Vec<Value> = serde_json::from_str::<Value>(&text(&v)).unwrap()["hits"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(hits[0]["kind"], "directive");
    assert_eq!(hits[1]["kind"], "fact");
}

#[tokio::test]
async fn documents_are_written_by_the_operator_read_by_the_agent_and_edits_conflict_honestly() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let (st, v) = call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/guide-workspace",
        Some(json!({"body": "# Workspace\n\nRun `just test`."})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hash = v["hash"].as_str().unwrap().to_string();
    assert_eq!(v["kind"], "guide");
    assert_eq!(v["title"], "Workspace");
    // Bad slugs are refused.
    let (st, _) = call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/Bad%20Slug",
        Some(json!({"body": "x"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(1, "doc_search", json!({"query": "just test"})),
    )
    .await;
    let hits = serde_json::from_str::<Value>(&text(&v)).unwrap();
    assert_eq!(hits["hits"][0]["slug"], "guide-workspace");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(2, "doc_read", json!({"slug": "guide-workspace"})),
    )
    .await;
    let doc = serde_json::from_str::<Value>(&text(&v)).unwrap();
    assert_eq!(doc["hash"], hash);
    assert!(doc["body"].as_str().unwrap().contains("just test"));
    // doc_write is not named by the bundle: it is asked, and with no live
    // session to carry the question it is refused rather than run.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            3,
            "doc_write",
            json!({"slug": "guide-workspace", "body": "gone"}),
        ),
    )
    .await;
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert_eq!(
        h.store
            .doc_get("personal", "guide-workspace")
            .unwrap()
            .unwrap()
            .body,
        "# Workspace\n\nRun `just test`."
    );

    // An edit against a stale hash is refused with the current state.
    let (st, v) = call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/guide-workspace",
        Some(json!({"body": "v2"})),
        &[("if-match", &hash)],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hash2 = v["hash"].as_str().unwrap().to_string();
    let (st, v) = call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/guide-workspace",
        Some(json!({"body": "v3"})),
        &[("if-match", &hash)],
    )
    .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    assert_eq!(v["hash"], hash2);
    assert_eq!(v["body"], "v2");
    // Same id throughout: the document evolved, it was not recreated.
    let (_, list) = call_with(&h.operator, "GET", "/api/docs?channel=personal", None, &[]).await;
    assert_eq!(list["docs"].as_array().unwrap().len(), 1);
    let (st, _) = call_with(
        &h.operator,
        "DELETE",
        "/api/docs/personal/guide-workspace",
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call_with(
        &h.operator,
        "GET",
        "/api/docs/personal/guide-workspace",
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_archived_document_is_kept_but_out_of_the_list_and_search_until_asked_for() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let (st, _) = call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/guide-live",
        Some(json!({"body": "# Live\n\nthe current way"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v) = call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/plan-old",
        Some(json!({"body": "# Old\n\nthe current way, once", "archived": true})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["archived"], 1);

    let slugs = |list: &Value| -> Vec<String> {
        list["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["slug"].as_str().unwrap().to_string())
            .collect()
    };
    let (_, list) = call_with(&h.operator, "GET", "/api/docs?channel=personal", None, &[]).await;
    assert_eq!(slugs(&list), ["guide-live"]);
    let (_, list) = call_with(
        &h.operator,
        "GET",
        "/api/docs?channel=personal&archived=true",
        None,
        &[],
    )
    .await;
    assert_eq!(slugs(&list).len(), 2);

    // Search leaves it out; reading it by slug does not.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(1, "doc_search", json!({"query": "current way"})),
    )
    .await;
    let hits = serde_json::from_str::<Value>(&text(&v)).unwrap();
    let found: Vec<&str> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["slug"].as_str().unwrap())
        .collect();
    assert_eq!(found, ["guide-live"]);
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(2, "doc_read", json!({"slug": "plan-old"})),
    )
    .await;
    let doc = serde_json::from_str::<Value>(&text(&v)).unwrap();
    assert_eq!(doc["archived"], true);

    // An edit that says nothing about it keeps it archived; saying so restores it.
    call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/plan-old",
        Some(json!({"body": "# Old\n\nedited"})),
        &[],
    )
    .await;
    assert_eq!(
        h.store
            .doc_get("personal", "plan-old")
            .unwrap()
            .unwrap()
            .archived,
        1
    );
    call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/plan-old",
        Some(json!({"body": "# Old\n\nedited", "archived": false})),
        &[],
    )
    .await;
    let (_, list) = call_with(&h.operator, "GET", "/api/docs?channel=personal", None, &[]).await;
    assert_eq!(slugs(&list).len(), 2);
}

/// The documents `tracon doc export` writes, by the CLI's own route: GET the
/// list, then GET each body. This breaks if the CLI's route breaks.
async fn exported(app: &axum::Router, channel: &str) -> Vec<tracon::corpus::export::ExportDoc> {
    let (st, list) = call_with(
        app,
        "GET",
        &format!("/api/docs?channel={channel}&archived=true"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{list}");
    let mut out = Vec::new();
    for d in list["docs"].as_array().cloned().unwrap_or_default() {
        if d["format"].as_str() == Some("html") {
            continue;
        }
        let slug = d["slug"].as_str().unwrap().to_string();
        let (_, full) = call_with(
            app,
            "GET",
            &format!("/api/docs/{channel}/{slug}"),
            None,
            &[],
        )
        .await;
        out.push(tracon::corpus::export::ExportDoc {
            slug,
            body: full["body"].as_str().unwrap().to_string(),
            archived: d["archived"].as_i64() == Some(1),
        });
    }
    out
}

/// Every document on a channel, as an operator comparing two nodes would read
/// it: the identity, the body, and the metadata hanging off it.
async fn corpus_of(app: &axum::Router, channel: &str) -> Vec<Value> {
    let (_, list) = call_with(
        app,
        "GET",
        &format!("/api/docs?channel={channel}&archived=true"),
        None,
        &[],
    )
    .await;
    let mut out = Vec::new();
    for d in list["docs"].as_array().cloned().unwrap_or_default() {
        let slug = d["slug"].as_str().unwrap().to_string();
        let (_, full) = call_with(
            app,
            "GET",
            &format!("/api/docs/{channel}/{slug}"),
            None,
            &[],
        )
        .await;
        out.push(json!({
            "slug": slug,
            "kind": d["kind"],
            "title": d["title"],
            "format": d["format"],
            "archived": d["archived"],
            "hash": full["hash"],
            "body": full["body"],
        }));
    }
    out.sort_by(|a, b| a["slug"].as_str().unwrap().cmp(b["slug"].as_str().unwrap()));
    out
}

/// The corpus is meant to outlive this binary, so its export has to be a
/// complete, plain-file description of it: export to a directory, import that
/// directory into a node that has never seen any of it, and the two corpora
/// agree byte for byte — bodies, and the metadata (kind, title, archived
/// state, content hash) derived from them.
#[tokio::test]
async fn a_document_export_imports_into_a_fresh_node_unchanged() {
    let from = harness().await;
    // Awkward on purpose: an archived document, a body with no heading, a slug
    // with no kind prefix, trailing whitespace and a missing final newline,
    // CRLF, and non-ASCII — each of which a format that normalised anything
    // would quietly change.
    let written: [(&str, &str, bool); 6] = [
        (
            "guide-workspace",
            "# Workspace\n\nRun `just test`.\n",
            false,
        ),
        ("note-odd", "no heading at all, and no final newline", false),
        (
            "ref-unicode",
            "# Ünicode ✅\n\nnaïve — em dash, é.\n",
            false,
        ),
        (
            "plan-crlf",
            "# Plan\r\n\r\ntrailing spaces here:   \n\n\n",
            false,
        ),
        ("scratch", "# Scratch\n\nno kind prefix.\n", false),
        ("meeting-old", "# Standup\n\nlast quarter.\n", true),
    ];
    for (slug, body, archived) in written {
        let (st, v) = call_with(
            &from.operator,
            "PUT",
            &format!("/api/docs/personal/{slug}"),
            Some(json!({ "body": body, "archived": archived })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{slug}: {v}");
    }
    let before = corpus_of(&from.operator, "personal").await;
    assert_eq!(before.len(), written.len());

    // Export: plain Markdown files, archived ones under archive/.
    let dir = state::scratch("document-export-round-trip");
    let docs = exported(&from.operator, "personal").await;
    let report = tracon::corpus::export::sync_dir(&dir, &docs).unwrap();
    assert_eq!(report.written, written.len());
    // Nothing but `<slug>.md` and `archive/<slug>.md`: no sidecar, no
    // manifest, no index. A directory of Markdown is the whole format.
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "archive",
            "guide-workspace.md",
            "note-odd.md",
            "plan-crlf.md",
            "ref-unicode.md",
            "scratch.md",
        ]
    );
    // The file on disk is the body and nothing else.
    for (slug, body, archived) in written {
        let path = if archived {
            dir.join("archive").join(format!("{slug}.md"))
        } else {
            dir.join(format!("{slug}.md"))
        };
        assert_eq!(std::fs::read(&path).unwrap(), body.as_bytes(), "{slug}");
    }

    // Import into a node that has never held any of this, addressed only by
    // filename: no ids, no channel, no origin node is needed to read it back.
    let (imported, skipped) = tracon::corpus::import::read_dir(&dir).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    let into = harness().await;
    for d in &imported {
        let (st, v) = call_with(
            &into.operator,
            "PUT",
            &format!("/api/docs/personal/{}", d.slug),
            Some(json!({ "body": d.body, "archived": d.archived })),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {v}", d.slug);
    }
    assert_eq!(corpus_of(&into.operator, "personal").await, before);
}

#[tokio::test]
async fn active_memories_are_browsed_edited_in_place_and_retired() {
    state::isolate();
    let h = harness().await;
    let (st, v) = call_with(
        &h.operator,
        "POST",
        "/api/memories",
        Some(json!({"channel": "personal", "body": "run just test before every commit"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let id = v["id"].as_str().unwrap().to_string();

    let (st, v) = call_with(
        &h.operator,
        "GET",
        "/api/memories?channel=personal&state=active",
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(
        v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == id),
        "{v}"
    );

    // Editing changes only the body.
    let (st, v) = call_with(
        &h.operator,
        "PATCH",
        &format!("/api/memories/{id}"),
        Some(json!({"body": "run just test --compact before every commit"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, v) = call_with(
        &h.operator,
        "GET",
        "/api/memories?channel=personal&state=active",
        None,
        &[],
    )
    .await;
    let row = v["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap();
    assert_eq!(row["body"], "run just test --compact before every commit");
    assert_eq!(
        row["kind"], "directive",
        "editing the body leaves the rest alone"
    );

    // An empty body is refused.
    let (st, _) = call_with(
        &h.operator,
        "PATCH",
        &format!("/api/memories/{id}"),
        Some(json!({"body": "   "})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Editing a memory that does not exist is a 404.
    let (st, _) = call_with(
        &h.operator,
        "PATCH",
        "/api/memories/does-not-exist",
        Some(json!({"body": "x"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Retiring removes it from the active list.
    let (st, _) = call_with(
        &h.operator,
        "DELETE",
        &format!("/api/memories/{id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, v) = call_with(
        &h.operator,
        "GET",
        "/api/memories?channel=personal&state=active",
        None,
        &[],
    )
    .await;
    assert!(
        !v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == id),
        "{v}"
    );
}
