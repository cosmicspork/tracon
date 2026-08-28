//! Watching the node's queue.
//!
//! The node already publishes everything the tray needs over SSE, so this is a
//! reader of the same stream the browser uses. No state is kept that the node
//! does not have: a dropped connection is a reconnect and a refetch.

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::StreamExt;
use serde::Deserialize;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::{node_url, tray, State};

/// One thing waiting on the operator, flattened across the queue's kinds so
/// the tray can render them in one list.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub label: String,
    /// Where the window should open.
    pub path: String,
    /// The session to kill, when killing makes sense for this kind.
    pub session_id: Option<String>,
}

#[derive(Default, Clone, Debug)]
pub struct Queue {
    pub waiting: Vec<Item>,
    /// Running sessions, for the tray's kill switch.
    pub running: Vec<Item>,
}

#[derive(Deserialize)]
struct Permission {
    id: String,
    session_id: String,
    title: String,
}

#[derive(Deserialize)]
struct Review {
    id: String,
    title: String,
    #[serde(default)]
    added: i64,
    #[serde(default)]
    removed: i64,
}

#[derive(Deserialize)]
struct Session {
    id: String,
    branch: String,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct Promotion {
    id: String,
    channel: String,
}

#[derive(Deserialize, Default)]
struct QueueBody {
    #[serde(default)]
    waiting: Vec<Permission>,
    #[serde(default)]
    reviews: Vec<Review>,
    #[serde(default)]
    promotions: Vec<Promotion>,
    #[serde(default)]
    running: Vec<Session>,
}

impl QueueBody {
    fn into_queue(self) -> Queue {
        let mut waiting = Vec::new();
        for p in self.waiting {
            waiting.push(Item {
                id: format!("perm:{}", p.id),
                label: p.title,
                path: format!("/sessions/{}", p.session_id),
                session_id: Some(p.session_id),
            });
        }
        for r in self.reviews {
            waiting.push(Item {
                id: format!("review:{}", r.id),
                label: format!("{} (+{} −{})", r.title, r.added, r.removed),
                path: format!("/reviews/{}", r.id),
                session_id: None,
            });
        }
        for p in self.promotions {
            waiting.push(Item {
                id: format!("promo:{}", p.id),
                label: format!("Memory promotions on {}", p.channel),
                path: format!("/promotions/{}", p.id),
                session_id: None,
            });
        }
        let running = self
            .running
            .into_iter()
            .filter(|s| s.state != "closed")
            .map(|s| Item {
                id: format!("session:{}", s.id),
                label: s.branch,
                path: format!("/sessions/{}", s.id),
                session_id: Some(s.id),
            })
            .collect();
        Queue { waiting, running }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Ask the node what is waiting right now.
async fn fetch(http: &reqwest::Client) -> Option<Queue> {
    let res = http
        .get(format!("{}/api/queue", node_url()))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    Some(res.json::<QueueBody>().await.ok()?.into_queue())
}

/// Announce anything that has appeared since the last look.
fn announce(app: &AppHandle, state: &State, queue: &Queue, first: bool) {
    let mut announced = state.announced.lock().unwrap();
    let present: HashSet<String> = queue.waiting.iter().map(|i| i.id.clone()).collect();
    // Anything gone is forgotten, so a re-ask under a new id is news again.
    announced.retain(|id| present.contains(id));

    let fresh: Vec<&Item> = queue
        .waiting
        .iter()
        .filter(|i| !announced.contains(&i.id))
        .collect();
    for i in &fresh {
        announced.insert(i.id.clone());
    }
    // The first look after connecting is the standing queue, not news: the
    // window shows it, and announcing it on every start trains you to dismiss.
    if first || fresh.is_empty() {
        return;
    }
    let body = if fresh.len() == 1 {
        fresh[0].label.clone()
    } else {
        format!("{} things are waiting on you", fresh.len())
    };
    let _ = app
        .notification()
        .builder()
        .title("tracon")
        .body(body)
        .show();
}

/// Follow the node's event stream, refetching the queue when it changes.
/// Returns only when the app is going away.
pub async fn watch(app: AppHandle, state: Arc<State>) {
    let http = client();
    let mut first = true;
    loop {
        // The stream says *that* something changed; the queue endpoint says
        // what. Fetching keeps one shape of the queue rather than two.
        if let Some(q) = fetch(&http).await {
            announce(&app, &state, &q, first);
            *state.queue.lock().unwrap() = q;
            *state.connected.lock().unwrap() = true;
            first = false;
            tray::refresh(&app, &state);
        }

        match http
            .get(format!("{}/api/stream", node_url()))
            .header("accept", "text/event-stream")
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => {
                let mut stream = res.bytes_stream();
                let mut buf = String::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(bytes) = chunk else { break };
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    // Only the event names matter here; the payload is fetched.
                    let interesting = buf.contains("event: queue")
                        || buf.contains("event: reviews")
                        || buf.contains("event: promotions")
                        || buf.contains("event: session");
                    // Keep the buffer bounded: this is a long-lived stream.
                    if buf.len() > 64 * 1024 {
                        buf.clear();
                    }
                    if interesting {
                        buf.clear();
                        if let Some(q) = fetch(&http).await {
                            announce(&app, &state, &q, false);
                            *state.queue.lock().unwrap() = q;
                            tray::refresh(&app, &state);
                        }
                    }
                }
            }
            _ => {}
        }

        // The stream ended: the node restarted, the machine slept, or it is
        // not running yet. None of those are errors worth surfacing.
        *state.connected.lock().unwrap() = false;
        tray::refresh(&app, &state);
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Kill a session from the tray.
pub async fn kill(session_id: String) {
    let http = client();
    let _ = http
        .post(format!("{}/api/sessions/{session_id}/kill", node_url()))
        .send()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(json: serde_json::Value) -> Queue {
        serde_json::from_value::<QueueBody>(json).unwrap().into_queue()
    }

    #[test]
    fn every_kind_of_waiting_lands_in_one_list_with_somewhere_to_go() {
        let q = body(serde_json::json!({
            "waiting": [{"id": "p1", "session_id": "s1", "title": "run just check"}],
            "reviews": [{"id": "r1", "title": "feat: the thing", "added": 12, "removed": 3}],
            "promotions": [{"id": "m1", "channel": "personal"}],
            "running": [{"id": "s1", "branch": "feat/thing", "state": "running"}],
        }));
        assert_eq!(q.waiting.len(), 3);
        assert_eq!(q.waiting[0].label, "run just check");
        assert_eq!(q.waiting[0].path, "/sessions/s1");
        assert_eq!(q.waiting[1].label, "feat: the thing (+12 −3)");
        assert_eq!(q.waiting[1].path, "/reviews/r1");
        assert_eq!(q.waiting[2].path, "/promotions/m1");
        // Ids are namespaced, so a permission and a review that share an id
        // are not mistaken for the same thing.
        assert_eq!(q.waiting[0].id, "perm:p1");
        assert_eq!(q.waiting[1].id, "review:r1");
    }

    /// A closed session cannot be killed, so the tray must not offer to.
    #[test]
    fn only_live_sessions_are_offered_for_killing() {
        let q = body(serde_json::json!({
            "running": [
                {"id": "s1", "branch": "feat/a", "state": "running"},
                {"id": "s2", "branch": "feat/b", "state": "closed"},
            ],
        }));
        assert_eq!(q.running.len(), 1);
        assert_eq!(q.running[0].session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn an_empty_queue_is_read_without_complaint() {
        let q = body(serde_json::json!({}));
        assert!(q.waiting.is_empty());
        assert!(q.running.is_empty());
    }

    /// A field the node adds later must not stop an older wrapper reading the
    /// rest of the queue.
    #[test]
    fn unknown_fields_are_ignored() {
        let q = body(serde_json::json!({
            "waiting": [{"id": "p1", "session_id": "s1", "title": "x", "invented_later": 7}],
            "something_new": [1, 2, 3],
        }));
        assert_eq!(q.waiting.len(), 1);
    }
}
