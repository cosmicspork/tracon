//! `GET /api/stream`. Replays persisted events after `Last-Event-ID`, then
//! follows live. A client crash is a reconnect, never lost work: session state
//! lives in the node, and the client resumes from the last id it saw.

use std::convert::Infallible;

use axum::{
    extract::State,
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_core::Stream;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use super::api::AppState;
use crate::stream::Frame;

const REPLAY_LIMIT: i64 = 2000;

pub async fn stream(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(-1);

    // Subscribe before replaying, so an event landing mid-replay is delivered
    // rather than dropped between the two.
    let live = BroadcastStream::new(s.manager.hub().subscribe());

    let mut replay: Vec<Frame> = Vec::new();
    if after >= 0 {
        if let Ok(rows) = s.store().all_events_after(after, REPLAY_LIMIT) {
            replay = rows
                .into_iter()
                .map(|r| Frame::Event {
                    seq: r.seq,
                    session_id: r.session_id,
                    kind: r.kind,
                    ref_id: r.ref_id,
                    payload: r.payload,
                    at_ms: r.at_ms,
                })
                .collect();
        }
    }

    let replayed_to = replay.last().and_then(|f| f.id()).unwrap_or(after);
    let replay = tokio_stream::iter(replay.into_iter().map(Ok));
    let live = live.filter_map(move |f| match f {
        Ok(frame) => {
            // Skip anything the replay already delivered.
            match frame.id() {
                Some(seq) if seq <= replayed_to => None,
                _ => Some(Ok(frame)),
            }
        }
        // A lagged client has missed frames; it refetches on the next snapshot.
        Err(_) => None,
    });

    let events = replay.chain(live).map(|r: Result<Frame, Infallible>| {
        let frame = r.unwrap_or_else(|e| match e {});
        let mut ev = Event::default().event(frame.name());
        if let Some(id) = frame.id() {
            ev = ev.id(id.to_string());
        }
        Ok(ev.json_data(&frame).unwrap_or_else(|_| Event::default()))
    });

    Sse::new(events).keep_alive(KeepAlive::default())
}
