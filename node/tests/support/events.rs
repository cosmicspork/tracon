//! Reading a harness's event stream in a test without guessing how long it
//! stays quiet: wait for the thing the test is about, bounded by one deadline.

#![allow(dead_code)]

use std::time::Duration;

use tokio::sync::mpsc;
use tracon::adapter::HarnessEvent;

/// A slow runner can take a while to spawn the fake; a hang should still
/// fail the test rather than the job.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// One line per event, so a failing assertion prints something readable.
pub fn label(ev: &HarnessEvent) -> Option<String> {
    Some(match ev {
        HarnessEvent::MessageChunk { text, .. } => format!("chunk:{text}"),
        HarnessEvent::ThoughtChunk { text, .. } => format!("thought:{text}"),
        HarnessEvent::ToolCall(t) => format!("tool_call:{}", t.title),
        HarnessEvent::ToolCallUpdate(t) => {
            format!("tool_update:{}", t.status.clone().unwrap_or_default())
        }
        HarnessEvent::Usage { .. } => "usage".into(),
        HarnessEvent::Models(m) => format!("models:{}", m.len()),
        HarnessEvent::Exited { .. } => "exited".into(),
        HarnessEvent::Permission { .. } => "permission".into(),
        _ => return None,
    })
}

/// Collect labels until the harness asks for a permission. Panics if it
/// exits or goes silent first: an ask that never comes is the failure.
pub async fn next_permission(
    rx: &mut mpsc::Receiver<HarnessEvent>,
    out: &mut Vec<String>,
) -> HarnessEvent {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no permission ask before the deadline; saw {out:?}"))
            .unwrap_or_else(|| panic!("harness channel closed; saw {out:?}"));
        if let HarnessEvent::Permission { .. } = ev {
            return ev;
        }
        if let HarnessEvent::Exited { .. } = ev {
            panic!("harness exited before asking; saw {out:?}");
        }
        if let Some(l) = label(&ev) {
            out.push(l);
        }
    }
}

/// Collect labels until `want` has been seen, then keep draining whatever is
/// already queued. Panics if `want` never arrives.
pub async fn drain_until(rx: &mut mpsc::Receiver<HarnessEvent>, out: &mut Vec<String>, want: &str) {
    if !out.iter().any(|l| l == want) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let ev = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .unwrap_or_else(|_| panic!("never saw {want:?}; saw {out:?}"))
                .unwrap_or_else(|| panic!("harness channel closed before {want:?}; saw {out:?}"));
            let l = label(&ev);
            if let Some(l) = l {
                out.push(l);
            }
            if out.last().is_some_and(|l| l == want) {
                break;
            }
        }
    }
    while let Ok(ev) = rx.try_recv() {
        if let Some(l) = label(&ev) {
            out.push(l);
        }
    }
}
