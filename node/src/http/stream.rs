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
use tokio::sync::broadcast::error::RecvError;
use tokio_stream::wrappers::ReceiverStream;

use super::api::AppState;
use crate::stream::Frame;

/// The replay is read in batches and paged until caught up, so a reconnect
/// across a long gap is filled completely rather than silently truncated.
const REPLAY_BATCH: i64 = 1000;

fn to_event(frame: &Frame) -> Result<Event, Infallible> {
    let mut ev = Event::default().event(frame.name());
    if let Some(id) = frame.id() {
        ev = ev.id(id.to_string());
    }
    Ok(ev.json_data(frame).unwrap_or_else(|_| Event::default()))
}

fn row_to_frame(r: crate::store::EventRow) -> Frame {
    Frame::Event {
        seq: r.seq,
        session_id: r.session_id,
        kind: r.kind,
        ref_id: r.ref_id,
        payload: r.payload,
        at_ms: r.at_ms,
    }
}

pub async fn stream(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(-1);

    // Subscribe before replaying, so an event landing mid-replay is buffered on
    // the receiver and delivered after, rather than dropped between the two.
    let mut live = s.manager.hub().subscribe();
    let store = s.store().clone();
    let shutdown = s.manager.hub().shutdown_token();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        // Replay persisted events, paged until caught up. A single capped read
        // would leave a silent hole before the live tail on a long reconnect.
        let mut cursor = after;
        if after >= 0 {
            loop {
                let rows = match store.all_events_after(cursor, REPLAY_BATCH) {
                    Ok(rows) => rows,
                    Err(_) => break,
                };
                let n = rows.len() as i64;
                for r in rows {
                    cursor = cursor.max(r.seq);
                    if tx.send(to_event(&row_to_frame(r))).await.is_err() {
                        return;
                    }
                }
                if n < REPLAY_BATCH {
                    break;
                }
            }
        }
        let replayed_to = cursor;

        // Follow live until the node shuts down, or a lag we cannot bridge.
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                r = live.recv() => match r {
                    Ok(frame) => {
                        // Skip persisted events the replay already delivered.
                        if frame.id().is_some_and(|seq| seq <= replayed_to) {
                            continue;
                        }
                        if tx.send(to_event(&frame)).await.is_err() {
                            break;
                        }
                    }
                    // A lagged client missed frames. End the stream so it
                    // reconnects with Last-Event-ID and the paged replay fills
                    // the gap — rather than silently dropping the missed frames
                    // and advancing past them.
                    Err(RecvError::Lagged(_)) => break,
                    Err(RecvError::Closed) => break,
                },
            }
        }
    });

    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}
