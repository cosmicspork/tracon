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

    // What the phase cannot work without stays loaded where a harness defers
    // MCP tools; the rest is loaded when the model asks for it.
    let meta = |name: &str| {
        v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .map(|t| t["_meta"].clone())
            .unwrap()
    };
    assert_eq!(
        meta("submit_review"),
        json!({ "anthropic/alwaysLoad": true })
    );
    assert_eq!(meta("doc_search"), Value::Null);
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
    // doc_write is not named by the bundle: it is held for the operator, and
    // nothing is written until they allow it.
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
    assert_ne!(v["result"]["isError"], true, "{v}");
    assert!(text(&v).contains("awaiting_operator"), "{v}");
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
async fn a_working_note_is_edited_in_place_by_exact_replacements_and_a_miss_writes_nothing() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let body = "# Status\n\nbuild: red\ndeploy: held\n";
    let (st, v) = call_with(
        &h.operator,
        "PUT",
        "/api/docs/personal/note-status",
        Some(json!({ "body": body })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hash = v["hash"].as_str().unwrap().to_string();
    let stored = |slug: &str| h.store.doc_get("personal", slug).unwrap().unwrap();

    // One edit misses: nothing is written, and the miss is named.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            1,
            "doc_edit",
            json!({ "slug": "note-status", "if_hash": hash, "edits": [
                { "old_str": "build: red", "new_str": "build: green" },
                { "old_str": "deploy: done", "new_str": "deploy: live" },
            ] }),
        ),
    )
    .await;
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(text(&v).contains("edits[1]"), "{v}");
    assert_eq!(stored("note-status").body, body);

    // A working note is written unattended, as doc_write would be.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            2,
            "doc_edit",
            json!({ "slug": "note-status", "if_hash": hash, "edits": [
                { "old_str": "build: red", "new_str": "build: green" },
                { "old_str": "deploy: held", "new_str": "deploy: live" },
            ] }),
        ),
    )
    .await;
    assert_ne!(v["result"]["isError"], true, "{v}");
    let done = serde_json::from_str::<Value>(&text(&v)).unwrap();
    assert_eq!(done["applied"], 2);
    assert_eq!(done["format"], "markdown");
    let now = stored("note-status");
    assert_eq!(now.body, "# Status\n\nbuild: green\ndeploy: live\n");
    assert_eq!(done["hash"], now.hash);

    // The hash it was edited from is stale now: the same edit conflicts.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            3,
            "doc_edit",
            json!({ "slug": "note-status", "if_hash": hash, "edits": [
                { "old_str": "green", "new_str": "amber" },
            ] }),
        ),
    )
    .await;
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert!(text(&v).contains(&now.hash), "{v}");
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
#[tokio::test]
async fn the_api_files_a_document_under_a_kind_added_after_it_was_written() {
    state::isolate();
    let mut cfg = tracon::config::Config::default();
    cfg.docs.kind.push(tracon::config::DocKind {
        prefix: "runbook".into(),
        label: "Runbook".into(),
        description: "how to run something here".into(),
        working_note: false,
    });
    let h = support::harness::harness_with(cfg).await;
    // Written by a node that knew no runbook kind, so stored as `other`.
    let body = "# Deploy\n\nRoll the hub behind the proxy.";
    h.store
        .write_document_change(
            "A",
            "personal",
            "runbook-deploy",
            "other",
            "Deploy",
            body,
            &tracon::corpus::hash_body(body),
            None,
            true,
            "doc-runbook",
            None,
            None,
        )
        .unwrap();

    let slugs = |list: &Value, key: &str| -> Vec<String> {
        list[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["slug"].as_str().unwrap().to_string())
            .collect()
    };
    let get = |uri: &'static str| call_with(&h.operator, "GET", uri, None, &[]);
    let (st, list) = get("/api/docs?channel=personal&kind=runbook").await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(slugs(&list, "docs"), ["runbook-deploy"]);
    assert_eq!(list["docs"][0]["kind"], "runbook");
    let (_, list) = get("/api/docs?channel=personal&kind=other").await;
    assert!(slugs(&list, "docs").is_empty(), "{list}");
    let (_, found) = get("/api/docs?channel=personal&kind=runbook&q=hub%20proxy").await;
    assert_eq!(slugs(&found, "hits"), ["runbook-deploy"]);
    let (_, found) = get("/api/docs?channel=personal&kind=other&q=hub%20proxy").await;
    assert!(slugs(&found, "hits").is_empty(), "{found}");

    // The kinds, in the order the Documents screen groups by: the
    // configured ones as written, the reserved ones, then `other`.
    let (st, kinds) = get("/api/docs/kinds").await;
    assert_eq!(st, StatusCode::OK, "{kinds}");
    let kinds = kinds["kinds"].as_array().unwrap();
    let order: Vec<&str> = kinds.iter().map(|k| k["kind"].as_str().unwrap()).collect();
    assert_eq!(
        order,
        [
            "guide",
            "ref",
            "architecture",
            "proposal",
            "repo",
            "note",
            "meeting",
            "inbox",
            "runbook",
            "plan",
            "shown",
            "other",
        ]
    );
    let runbook = &kinds[8];
    assert_eq!(
        runbook,
        &json!({
            "kind": "runbook",
            "label": "Runbook",
            "description": "how to run something here",
            "working_note": false,
            "reserved": false,
        })
    );
    assert_eq!(kinds[5]["working_note"], true);
    assert_eq!(kinds[10]["label"], "Shown work");
    assert!(
        kinds[9..].iter().all(|k| k["reserved"] == true),
        "{kinds:?}"
    );
}

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
    let kinds = tracon::config::default_doc_kinds();
    let report = tracon::corpus::export::sync_dir(&dir, &docs, &kinds).unwrap();
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
    let (imported, skipped) = tracon::corpus::import::read_dir(&dir, &kinds).unwrap();
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

/// A correction names the memory it replaces. It always waits for the
/// operator, recall marks the old memory meanwhile, and promoting it retires
/// the old one while rejecting leaves it live.
#[tokio::test]
async fn a_retained_correction_waits_for_the_operator_and_promoting_it_retires_the_old_memory() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let retain = |id: u64, args: Value| {
        let (app, token) = (h.harness.clone(), token.clone());
        async move { mcp(&app, "s1", &token, tool_call(id, "retain", args)).await }
    };
    let made = |v: &Value| {
        serde_json::from_str::<Value>(&text(v)).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let v = retain(1, json!({"kind": "fact", "scope": "global", "body": "the gateway tests need the fixture registry in egress", "confidence": 0.95})).await;
    let old = made(&v);
    let v = retain(2, json!({"kind": "fact", "scope": "global", "body": "the deploy runs from the release branch", "confidence": 0.95})).await;
    let kept = made(&v);

    // A confident fact would be active on its own; as a correction it waits.
    let v = retain(3, json!({"kind": "fact", "scope": "global", "body": "the gateway tests vendor the fixture registry now", "confidence": 0.95, "supersedes": old})).await;
    assert_eq!(v["result"]["isError"], false, "{v}");
    let out: Value = serde_json::from_str(&text(&v)).unwrap();
    assert_eq!(out["state"], "candidate", "{out}");
    assert_eq!(out["supersedes"], old.as_str());
    let fix = out["id"].as_str().unwrap().to_string();
    // Without a scope, a correction reaches as far as what it replaces: this
    // session has no project, so the default scope would be refused.
    let v = retain(4, json!({"kind": "fact", "body": "the deploy runs from main", "confidence": 0.95, "supersedes": kept})).await;
    assert_eq!(v["result"]["isError"], false, "{v}");
    let wrong = made(&v);

    // Refused before anything is written: a directive, another channel's
    // memory, or one that does not exist.
    let (_, v) = call_with(
        &h.operator,
        "POST",
        "/api/memories",
        Some(json!({"channel": "personal", "body": "never push to main"})),
        &[],
    )
    .await;
    let directive = v["id"].as_str().unwrap().to_string();
    let (_, v) = call_with(
        &h.operator,
        "POST",
        "/api/memories",
        Some(json!({"channel": "work", "kind": "fact", "body": "work fact", "state": "active"})),
        &[],
    )
    .await;
    let elsewhere = v["id"].as_str().unwrap().to_string();
    // A memory still waiting for the operator is not one recall returns, so
    // it cannot be superseded either.
    let v = retain(
        5,
        json!({"kind": "lesson", "scope": "global", "body": "a lesson still waiting"}),
    )
    .await;
    let waiting = made(&v);
    for (i, target) in [
        directive.as_str(),
        elsewhere.as_str(),
        "no-such-memory",
        waiting.as_str(),
    ]
    .into_iter()
    .enumerate()
    {
        let v = retain(10 + i as u64, json!({"kind": "fact", "scope": "global", "body": "refused correction", "supersedes": target})).await;
        assert_eq!(v["result"]["isError"], true, "{target}: {v}");
    }
    let (_, v) = call_with(
        &h.operator,
        "GET",
        "/api/memories?channel=personal",
        None,
        &[],
    )
    .await;
    let rows = v["memories"].as_array().unwrap();
    assert!(
        !rows.iter().any(|m| m["body"] == "refused correction"),
        "{v}"
    );
    let row = rows.iter().find(|m| m["id"] == fix.as_str()).unwrap();
    assert_eq!(row["supersedes"], old.as_str(), "the link is stored");
    let row = rows.iter().find(|m| m["id"] == wrong.as_str()).unwrap();
    assert_eq!(row["scope"], "global", "the scope of what it replaces");

    // Until it is decided, the old memory is still recalled, marked.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(20, "recall", json!({"query": "gateway fixture registry"})),
    )
    .await;
    let hits: Value = serde_json::from_str(&text(&v)).unwrap();
    let hit = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == old.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{hits}"));
    assert_eq!(hit["correction_pending"], fix.as_str());

    // The batch shows each correction beside what it replaces.
    let (_, v) = call_with(&h.operator, "POST", "/api/promotions/batch", None, &[]).await;
    let pid = v["created"][0].as_str().unwrap().to_string();
    let (_, p) = call_with(
        &h.operator,
        "GET",
        &format!("/api/promotions/{pid}"),
        None,
        &[],
    )
    .await;
    let item = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["memory_id"] == fix.as_str())
        .cloned()
        .unwrap();
    assert_eq!(item["supersedes"], old.as_str());
    assert_eq!(
        item["replaces"]["body"],
        "the gateway tests need the fixture registry in egress"
    );

    // One decision each: the promoted correction retires the old memory, the
    // rejected one leaves its target live.
    let (st, v) = call_with(
        &h.operator,
        "POST",
        &format!("/api/promotions/{pid}/verdict"),
        Some(json!({"verdicts": {fix.clone(): "promote", wrong.clone(): "reject"}})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, v) = call_with(
        &h.operator,
        "GET",
        "/api/memories?channel=personal",
        None,
        &[],
    )
    .await;
    let state_of = |id: &str| {
        v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .map(|m| m["state"].as_str().unwrap().to_string())
            .unwrap()
    };
    assert_eq!(state_of(&old), "retired");
    assert_eq!(state_of(&fix), "promoted");
    assert_eq!(state_of(&kept), "active");
    assert_eq!(state_of(&wrong), "rejected");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(21, "recall", json!({"query": "gateway fixture registry"})),
    )
    .await;
    let hits: Value = serde_json::from_str(&text(&v)).unwrap();
    let ids: Vec<&str> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![fix.as_str()], "one memory where there were two");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(22, "recall", json!({"query": "deploy release branch"})),
    )
    .await;
    let hits: Value = serde_json::from_str(&text(&v)).unwrap();
    let hit = &hits["hits"][0];
    assert_eq!(hit["id"], kept.as_str());
    assert!(
        hit.get("correction_pending").is_none(),
        "a rejected correction is no longer pending: {hit}"
    );
}
