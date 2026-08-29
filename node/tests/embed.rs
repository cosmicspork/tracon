//! Building a vector index against a stub embedding endpoint.
//!
//! The stub is deterministic — it turns text into a vector by counting a few
//! marker words — so "the nearest neighbour is the right document" is a claim
//! about the indexing and search path rather than about a model.

use std::sync::{Arc, Mutex};

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};

use tracon::{
    config::Config,
    embed::Embedder,
    store::{now_ms, Store},
};

const DIM: usize = 4;
/// Each dimension is a topic. Two texts about the same topic point the same
/// way whether or not they share any words, which is the property that makes
/// this a fair stand-in for an embedding model.
const TOPICS: [&[&str]; DIM] = [
    &["test", "testing", "pest", "check", "suite"],
    &["deploy", "deploys", "release", "ship", "merge"],
    &["review", "approve", "verdict", "diff"],
    &["memory", "recall", "corpus", "document"],
];

#[derive(Clone, Default)]
struct Stub {
    seen: Arc<Mutex<Vec<Value>>>,
    fail: Arc<Mutex<bool>>,
    /// Return the wrong number of dimensions, to prove that is caught.
    wrong_dim: Arc<Mutex<bool>>,
}

async fn embeddings(
    State(s): State<Stub>,
    Json(v): Json<Value>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    if *s.fail.lock().unwrap() {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }
    s.seen.lock().unwrap().push(v.clone());
    let inputs: Vec<String> = v["input"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().unwrap_or_default().to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let wrong = *s.wrong_dim.lock().unwrap();
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let mut v: Vec<f32> = TOPICS
                .iter()
                .map(|words| {
                    words
                        .iter()
                        .map(|w| text.matches(w).count() as f32)
                        .sum::<f32>()
                })
                .collect();
            if v.iter().all(|x| *x == 0.0) {
                v[3] = 0.5;
            }
            if wrong {
                v.push(1.0);
            }
            json!({ "index": i, "embedding": v })
        })
        .collect();
    Ok(Json(json!({ "data": data })))
}

async fn stub() -> (Stub, String) {
    let s = Stub::default();
    let app = Router::new()
        .route("/v1/embeddings", post(embeddings))
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (s, format!("http://{addr}"))
}

fn config(base_url: &str) -> Arc<Config> {
    let mut cfg = Config::default();
    cfg.embed.enabled = true;
    cfg.embed.base_url = base_url.into();
    cfg.embed.model = "stub-embed".into();
    cfg.embed.dim = DIM;
    cfg.embed.batch = 2;
    Arc::new(cfg)
}

fn doc(store: &Store, _id: &str, channel: &str, slug: &str, title: &str, body: &str) -> String {
    match store
        .write_document_change(
            "n1", channel, slug, "guide", title, body, "h", None, false, slug,
        )
        .unwrap()
    {
        tracon::store::corpus::DocumentWrite::Written { row, .. } => row.id,
        other => panic!("document write refused: {other:?}"),
    }
}

fn memory(store: &Store, channel: &str, id: &str, body: &str) {
    memory_kind(store, channel, id, "fact", body)
}

fn memory_kind(store: &Store, channel: &str, id: &str, kind: &str, body: &str) {
    memory_aged(store, channel, id, kind, body, now_ms())
}

/// `created_ms` matters: a fact's rank decays on a 90-day half-life, so a
/// fresh confident fact sits level with a directive and an old one does not.
fn memory_aged(store: &Store, channel: &str, id: &str, kind: &str, body: &str, created_ms: i64) {
    store
        .write_change(
            "n1",
            channel,
            "memory",
            tracon_sync::ChangeOp::Upsert,
            id,
            serde_json::json!({
                "channel": channel,
                "scope": "global",
                "scope_ref": Value::Null,
                "kind": kind,
                "body": body,
                "source_session": Value::Null,
                "source_node": Value::Null,
                "confidence": 1.0,
                "state": "active",
                "created_ms": created_ms,
                "updated_ms": created_ms,
            }),
        )
        .unwrap();
}

async fn indexed(e: &Embedder, store: &Store) -> usize {
    let mut n = 0;
    for (table, id, channel, body) in store.rows_to_embed().unwrap() {
        n += e
            .index_record(&table, &id, &channel, &body, "t")
            .await
            .unwrap();
    }
    n
}

fn setup(base: &str) -> (Arc<Store>, Embedder) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.vec_ensure(DIM).unwrap();
    let e = Embedder::new(config(base), store.clone(), reqwest::Client::new());
    (store, e)
}

#[tokio::test]
async fn a_query_finds_the_document_that_means_the_same_thing() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    let d1 = doc(&store, "d1", "personal", "guide-testing", "Running the suite",
        "# Running the suite\n\nRun pest to check the whole workspace before you push anything at all.\n");
    let d2 = doc(&store, "d2", "personal", "guide-shipping", "Shipping",
        "# Shipping\n\nMerging to main deploys. A release goes out on every merge without further ceremony.\n");
    assert!(indexed(&e, &store).await >= 2);

    // The query shares no words with the document, which is the whole point:
    // FTS could not answer this one.
    let q = e
        .embed_query("how do I run the check suite", "t")
        .await
        .unwrap();
    let hits = store.vec_search(Some("personal"), &q, 3).unwrap();
    assert_eq!(hits[0].source_id, d1, "{hits:#?}");

    let q = e.embed_query("what ships a release", "t").await.unwrap();
    let hits = store.vec_search(Some("personal"), &q, 3).unwrap();
    assert_eq!(hits[0].source_id, d2, "{hits:#?}");
}

/// A hit points at a span of the body, so the interface can show the paragraph
/// that matched rather than the top of the file.
#[tokio::test]
async fn a_hit_points_at_the_text_it_matched() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    let body = "# Guide\n\nSomething unrelated entirely, at some length so it stands as its own section.\n\n\
                ## Review\n\nAn approve or a verdict on the diff is how a change lands here.\n";
    let d1 = doc(&store, "d1", "personal", "guide-x", "Guide", body);
    indexed(&e, &store).await;

    let q = e.embed_query("approve the diff", "t").await.unwrap();
    let hits = store.vec_search(Some("personal"), &q, 1).unwrap();
    let row = store.doc_by_id(&d1).unwrap().unwrap();
    // The document is embedded as title + body, so offsets index that text.
    let text = format!("{}\n\n{}", row.title, row.body);
    let span = &text[hits[0].offset as usize..(hits[0].offset + hits[0].len) as usize];
    assert!(span.contains("verdict"), "matched span was {span:?}");
}

/// Sweeping an unchanged corpus must cost nothing, or every replicated change
/// re-embeds the whole corpus.
#[tokio::test]
async fn nothing_is_re_embedded_when_nothing_changed() {
    let (s, base) = stub().await;
    let (store, e) = setup(&base);
    doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nTesting the suite with pest, at enough length to be its own section.\n",
    );
    assert!(indexed(&e, &store).await > 0);
    let after_first = s.seen.lock().unwrap().len();

    assert_eq!(indexed(&e, &store).await, 0, "a second sweep re-embedded");
    assert_eq!(
        s.seen.lock().unwrap().len(),
        after_first,
        "it called the endpoint again"
    );
}

#[tokio::test]
async fn an_edit_re_embeds_and_leaves_no_stale_vector() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nTesting the suite with pest, at enough length to be its own section.\n",
    );
    indexed(&e, &store).await;
    let before = store.vec_count().unwrap();

    let d1 = doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nDeploy on merge, release every time, at enough length to be its own section.\n",
    );
    assert!(indexed(&e, &store).await > 0, "an edit did not re-embed");
    assert_eq!(
        store.vec_count().unwrap(),
        before,
        "the old chunks were left behind"
    );

    let q = e.embed_query("release and deploy", "t").await.unwrap();
    let hits = store.vec_search(Some("personal"), &q, 1).unwrap();
    assert_eq!(hits[0].source_id, d1);
}

/// A deleted document must not stay findable. The tombstone drops it from
/// `rows_to_embed`, so the sweep alone is not enough — the index has to be
/// cleared for it too.
#[tokio::test]
async fn deleting_a_document_removes_its_vectors() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    let d1 = doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nTesting the suite with pest, at enough length to be its own section.\n",
    );
    indexed(&e, &store).await;
    assert!(store.vec_count().unwrap() > 0);

    store.vec_forget("document", &d1).unwrap();
    assert_eq!(store.vec_count().unwrap(), 0);
    let q = e.embed_query("testing the suite", "t").await.unwrap();
    assert!(store
        .vec_search(Some("personal"), &q, 5)
        .unwrap()
        .is_empty());
}

/// An endpoint that is not running is the normal case on a node where nobody
/// started one. It must cost search quality and nothing else.
#[tokio::test]
async fn a_dead_endpoint_is_an_error_not_a_panic() {
    let (store, e) = setup("http://127.0.0.1:1");
    doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nSome text long enough to be a section of its own here.\n",
    );
    let err = e
        .index_record("document", "d1", "personal", "body text here", "t")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("embedding endpoint"), "{err}");
    // FTS is untouched: the corpus is still searchable as text.
    assert!(!store
        .doc_search(Some("personal"), None, "text", 5)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_failing_endpoint_leaves_the_index_consistent() {
    let (s, base) = stub().await;
    let (store, e) = setup(&base);
    doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nTesting the suite with pest, at enough length to be its own section.\n",
    );
    indexed(&e, &store).await;
    let good = store.vec_count().unwrap();

    *s.fail.lock().unwrap() = true;
    let d1 = doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nDeploying and releasing, at enough length to be its own section here.\n",
    );
    assert!(e
        .index_record("document", &d1, "personal", "new body", "t")
        .await
        .is_err());
    // The stale vectors are dropped rather than left claiming to describe text
    // that no longer exists: fewer results, never wrong ones.
    assert!(store.vec_count().unwrap() < good || store.vec_count().unwrap() == 0);
}

/// A model whose dimension does not match the configuration would silently
/// produce an index that cannot be searched. Catch it at the first call.
#[tokio::test]
async fn a_dimension_mismatch_is_refused() {
    let (s, base) = stub().await;
    let (store, e) = setup(&base);
    *s.wrong_dim.lock().unwrap() = true;
    doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nSome text long enough to be a section of its own here.\n",
    );
    let err = e
        .index_record("document", "d1", "personal", "body", "t")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("dimension"), "{err}");
    assert_eq!(store.vec_count().unwrap(), 0);
}

#[tokio::test]
async fn a_memory_is_indexed_as_one_chunk_on_its_own_channel() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    memory(&store, "work", "m1", "the check command is pest");
    let rows = store.rows_to_embed().unwrap();
    assert!(rows.iter().any(|(t, ..)| t == "memory"));
    indexed(&e, &store).await;

    let q = e.embed_query("how do I run the suite", "t").await.unwrap();
    assert!(store
        .vec_search(Some("personal"), &q, 5)
        .unwrap()
        .is_empty());
    assert!(!store.vec_search(Some("work"), &q, 5).unwrap().is_empty());
}

/// The claim the ROADMAP set as the bar for doing this at all: a question FTS
/// cannot answer, because it shares no words with the document that answers it.
#[tokio::test]
async fn a_query_fts_cannot_answer_is_answered_by_the_vector_leg() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    doc(&store, "d1", "personal", "guide-testing", "Running the suite",
        "# Running the suite\n\nRun pest before pushing. It is the whole workspace, and nothing else runs it.\n");
    doc(&store, "d2", "personal", "guide-noise", "Unrelated",
        "# Unrelated\n\nA document about deploying and releasing, which shares no ideas with the other one.\n");
    indexed(&e, &store).await;

    // No word here appears in either document.
    let question = "how do I check my work";
    assert!(
        store
            .doc_search(Some("personal"), None, question, 5)
            .unwrap()
            .is_empty(),
        "FTS answered it after all; the test no longer proves anything"
    );

    let near = e
        .embed_query(question, "t")
        .await
        .map(|v| store.vec_search(Some("personal"), &v, 8).unwrap())
        .unwrap();
    let hits = store
        .doc_search_hybrid(Some("personal"), None, question, 5, &near)
        .unwrap();
    assert_eq!(
        hits.first().map(|h| h.slug.clone().unwrap()),
        Some("guide-testing".into()),
        "{hits:#?}"
    );
    // And the hit carries the paragraph it matched, not the top of the file.
    assert!(hits[0].text.contains("pest"), "{:?}", hits[0].text);
}

/// Directives are the operator's standing instructions, and outrank a fact
/// however good the fact's vector is. Vectors reorder within that decision,
/// they do not overturn it.
#[tokio::test]
async fn a_semantic_match_does_not_outrank_the_operators_own_directive() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    memory_kind(
        &store,
        "personal",
        "m-directive",
        "directive",
        "always run the test suite before pushing",
    );
    // A year old, so its tier is genuinely below the directive's: a *fresh*
    // confident fact sits level with one by design, and would not test this.
    memory_aged(
        &store,
        "personal",
        "m-fact",
        "fact",
        "the check suite is run with pest and testing takes a while",
        now_ms() - 365 * 24 * 3600 * 1000,
    );
    indexed(&e, &store).await;

    let near = e
        .embed_query("running the test suite", "t")
        .await
        .map(|v| store.vec_search(Some("personal"), &v, 8).unwrap())
        .unwrap();
    assert!(!near.is_empty(), "nothing was indexed to fuse");
    let hits = store
        .recall_hybrid("personal", "test suite", None, None, None, 8, &near)
        .unwrap();
    assert_eq!(hits[0].id, "m-directive", "{hits:#?}");
}

/// A semantic match must not become a way to read another project's memory:
/// the vector leg goes through the same scope predicate as the text one.
#[tokio::test]
async fn the_vector_leg_respects_memory_scope() {
    let (_stub, base) = stub().await;
    let (store, e) = setup(&base);
    store
        .write_change(
            "n1",
            "personal",
            "memory",
            tracon_sync::ChangeOp::Upsert,
            "m-other",
            serde_json::json!({
                "channel": "personal", "scope": "project", "scope_ref": "someone-elses-project",
                "kind": "fact", "body": "the check command here is pest",
                "source_session": Value::Null, "source_node": Value::Null,
                "confidence": 1.0, "state": "active",
                "created_ms": now_ms(), "updated_ms": now_ms(),
            }),
        )
        .unwrap();
    indexed(&e, &store).await;

    let near = e
        .embed_query("how do I run the checks", "t")
        .await
        .map(|v| store.vec_search(Some("personal"), &v, 8).unwrap())
        .unwrap();
    assert!(!near.is_empty(), "the memory was not indexed at all");
    let hits = store
        .recall_hybrid(
            "personal",
            "checks",
            Some("my-project"),
            None,
            None,
            8,
            &near,
        )
        .unwrap();
    assert!(
        hits.iter().all(|h| h.id != "m-other"),
        "a scoped memory leaked through the vector leg: {hits:#?}"
    );
}

/// With no vectors the hybrid path must be the text path, exactly.
#[tokio::test]
async fn no_vectors_is_the_text_only_ranking_unchanged() {
    let (_stub, base) = stub().await;
    let (store, _e) = setup(&base);
    doc(
        &store,
        "d1",
        "personal",
        "guide-a",
        "A",
        "# A\n\nRun pest to test the suite, at enough length to be its own section.\n",
    );
    let plain = store.doc_search(Some("personal"), None, "pest", 5).unwrap();
    let hybrid = store
        .doc_search_hybrid(Some("personal"), None, "pest", 5, &[])
        .unwrap();
    assert_eq!(plain, hybrid);
    assert!(!plain.is_empty());
}
