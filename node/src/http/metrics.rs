//! The Prometheus scrape target on `[metrics] listen`: `GET /metrics` and
//! nothing else. Unauthenticated, read-only and taking no input, so it is a
//! router of its own and never the operator's; the harness NetworkPolicy does
//! not admit its port, so a harness cannot read it either.

use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::session::state::SessionState;
use crate::store::Store;
use crate::telemetry::{telemetry, SessionSeries};

#[derive(Clone)]
struct MetricsState {
    store: Arc<Store>,
    node_id: String,
}

pub fn router(store: Arc<Store>, node_id: String) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(MetricsState { store, node_id })
}

async fn scrape(axum::extract::State(state): axum::extract::State<MetricsState>) -> Response {
    let counted = tokio::task::spawn_blocking(move || {
        count_sessions(&state.store, &state.node_id);
        telemetry().encode()
    })
    .await;
    match counted {
        Ok(body) => (
            [(header::CONTENT_TYPE, crate::telemetry::CONTENT_TYPE)],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `tracon_sessions`, from the store: this node's sessions that have not
/// ended, by state and series. Read on every scrape rather than kept, so a
/// restart cannot leave it wrong.
pub fn count_sessions(store: &Store, node_id: &str) {
    let Ok(rows) = store.list_sessions(None) else {
        return;
    };
    let mut counts = std::collections::HashMap::<(SessionSeries, String), i64>::new();
    for row in rows {
        if row.node_id != node_id || SessionState::from_stored(&row.state).is_terminal() {
            continue;
        }
        *counts
            .entry((SessionSeries::of_row(store, &row), row.state))
            .or_default() += 1;
    }
    telemetry().set_sessions(
        counts
            .into_iter()
            .map(|((series, state), count)| (series, state, count)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn session_row(id: &str, state: &str) -> crate::store::SessionRow {
        crate::store::SessionRow {
            id: id.into(),
            node_id: "n-metrics".into(),
            channel: "@ops".into(),
            work_item_id: None,
            repo_path: "/home/someone/src/Cosmic Spork".into(),
            worktree_path: Some("/home/someone/worktrees/x".into()),
            branch: "acme-client/ticket-4521".into(),
            harness_id: "claude".into(),
            harness_version: "1".into(),
            harness_agent: None,
            harness_found: None,
            harness_protocol: None,
            harness_session_id: None,
            container_name: Some(format!("tracon-h-{id}")),
            model: "m".into(),
            project_id: None,
            phase: "plan".into(),
            policy_version: None,
            manifest_digest: None,
            review_id: None,
            budget_tokens: 0,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: state.into(),
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: 0,
            started_mono_ms: None,
            ended_mono_ms: None,
            updated_ms: 0,
            archived_ms: None,
            legacy_ms: None,
            parent_session: None,
            continued_from: None,
        }
    }

    fn store() -> Arc<Store> {
        let store = Store::open_in_memory().unwrap();
        store
            .put_node(
                &crate::store::NodeRow::from_json(
                    &serde_json::json!({ "id": "n-metrics", "harness": { "id": "claude" } }),
                )
                .unwrap(),
            )
            .unwrap();
        Arc::new(store)
    }

    async fn get(app: Router, path: &str) -> (StatusCode, Option<String>, String) {
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    /// The scrape answers without a credential, in the encoder's format, and
    /// describes every family; nothing else is served on this listener.
    #[tokio::test]
    async fn only_metrics_is_served_and_it_needs_no_credential() {
        let app = router(store(), "n-metrics".into());
        let (status, content_type, body) = get(app.clone(), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            content_type.as_deref(),
            Some(crate::telemetry::CONTENT_TYPE)
        );
        for family in crate::telemetry::tests::FAMILIES {
            assert!(body.contains(&format!("# HELP {family} ")), "{family}");
            assert!(body.contains(&format!("# TYPE {family} ")), "{family}");
        }
        // Exposition text: every line a comment or `name{labels} value`.
        for line in body.lines().filter(|line| !line.starts_with('#')) {
            let (series, value) = line.rsplit_once(' ').unwrap_or_else(|| panic!("{line}"));
            assert!(series.starts_with("tracon_"), "{line}");
            assert!(value.parse::<f64>().is_ok(), "{line}");
        }
        assert!(body.ends_with("# EOF\n"));

        for path in ["/", "/api/sessions", "/api/metrics", "/metrics/x"] {
            let (status, _, _) = get(app.clone(), path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    /// What is recorded shows up under the labels it was recorded with, and
    /// nothing that identifies one session, branch or path does.
    #[tokio::test]
    async fn recorded_work_appears_by_its_labels_and_nothing_identifying_does() {
        let store = store();
        store
            .insert_session(&session_row("sess-0199aa-metrics", "running"))
            .unwrap();
        store
            .insert_session(&session_row("sess-0199bb-metrics", "closed"))
            .unwrap();
        let series = SessionSeries::new("plan", "claude", Some("Cosmic Spork"), Some("@ops"));
        telemetry().session_started(&series, "harness", std::time::Duration::from_secs(3));
        telemetry().check_finished(
            "review",
            Some("Cosmic Spork"),
            "passed",
            Some(std::time::Duration::from_secs(40)),
        );

        let (_, _, body) = get(router(store, "n-metrics".into()), "/metrics").await;
        for line in [
            r#"tracon_session_starts_total{outcome="started",phase="plan",harness="claude",repo="cosmic-spork",channel="ops"}"#,
            r#"tracon_checks_total{origin="review",repo="cosmic-spork",outcome="passed"}"#,
            r#"tracon_sessions{state="running",phase="plan",harness="claude",repo="cosmic-spork",channel="ops"} 1"#,
        ] {
            assert!(body.contains(line), "{line}\n{body}");
        }
        // A closed session is not counted as one the node holds.
        assert!(
            !body.contains(r#"tracon_sessions{state="closed""#),
            "{body}"
        );
        for secret in [
            "sess-0199",
            "acme-client",
            "ticket-4521",
            "/home/someone",
            "tracon-h-",
        ] {
            assert!(!body.contains(secret), "{secret} in\n{body}");
        }
    }

    /// A continued session's path is `workspace://<session>`; its series
    /// name the repository the work came from, in every family, and never
    /// the session it continued.
    #[test]
    fn a_continued_session_is_counted_under_its_origin_repository() {
        let store = store();
        let origin = "0199cc4e-9f1a-4c3b-8e2d-5a6f7b8c9d0e";
        let continued = "0199dd4e-9f1a-4c3b-8e2d-5a6f7b8c9d0e";
        store
            .insert_session(&session_row(origin, "closed"))
            .unwrap();
        let mut row = session_row(continued, "running");
        row.repo_path = format!("workspace://{origin}");
        store.insert_session(&row).unwrap();
        let mut again = session_row("0199ee-again", "running");
        again.repo_path = format!("workspace://{continued}");
        store.insert_session(&again).unwrap();

        for row in [&row, &again] {
            // `session_series`, for starts and ends, and `of_row`, for
            // `tracon_sessions`, name it the same way.
            assert_eq!(
                crate::telemetry::session_repo(&store, &row.id).as_deref(),
                Some("Cosmic Spork")
            );
            let series = SessionSeries::of_row(&store, row);
            assert_eq!(series.repo, "cosmic-spork");
            assert!(!series.repo.contains("0199"), "{series:?}");
        }
        // A workspace that leads nowhere is left unnamed, not named by its id.
        let mut lost = session_row("0199ff-lost", "running");
        lost.repo_path = "workspace://0199ff-gone".into();
        store.insert_session(&lost).unwrap();
        assert_eq!(SessionSeries::of_row(&store, &lost).repo, "");
    }
}
