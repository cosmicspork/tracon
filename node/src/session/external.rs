//! A harness the operator runs themselves, calling a channel's tools through
//! the operator door.
//!
//! Such a harness has no session: every call stands alone, is logged to the
//! channel's external log under the lane the harness gives, and is fenced by
//! the channel's Stop. Before that, each client attached as a session row;
//! those rows remain, closed, and are recognised by [`HARNESS_ID`].

/// The `harness_id` an attached session recorded. Not an adapter: nothing is
/// launched, and `adapter_for` never sees it. The interface keys its wording
/// off this for old rows, and `recent_repos` filters on it.
pub const HARNESS_ID: &str = "external";

use std::collections::HashMap;

/// Which harness processes have called under each lane, so the interface can
/// say whether a lane is still running. Display only: a pid comes from a
/// header any caller can send, and nothing is decided by it.
///
/// A process is identified by its pid and its start time together, so a pid
/// the system has since reused for something else does not read as running.
/// It is only meaningful because the operator door is on the operator's own
/// machine: a pid from anywhere else is simply never found.
#[derive(Default)]
pub struct Liveness {
    seen: tokio::sync::Mutex<HashMap<LaneKey, HashMap<u32, String>>>,
}

/// A channel and the label called under there.
type LaneKey = (String, Option<String>);

impl Liveness {
    /// Remember `pid` under the lane, the first time it calls.
    pub async fn note(&self, channel: &str, lane: Option<&str>, pid: u32) {
        let key = (channel.to_string(), lane.map(str::to_string));
        if self
            .seen
            .lock()
            .await
            .get(&key)
            .is_some_and(|pids| pids.contains_key(&pid))
        {
            return;
        }
        if let Some(started) = started(pid).await {
            self.seen
                .lock()
                .await
                .entry(key)
                .or_default()
                .insert(pid, started);
        }
    }

    /// How many of the lane's processes are still running, forgetting the
    /// ones that are gone. `None` when no process has said which it is.
    pub async fn running(&self, channel: &str, lane: Option<&str>) -> Option<usize> {
        let key = (channel.to_string(), lane.map(str::to_string));
        let pids: Vec<(u32, String)> = self
            .seen
            .lock()
            .await
            .get(&key)?
            .iter()
            .map(|(pid, started)| (*pid, started.clone()))
            .collect();
        let mut alive = Vec::new();
        for (pid, was) in pids {
            if started(pid).await.as_deref() == Some(was.as_str()) {
                alive.push(pid);
            }
        }
        if let Some(known) = self.seen.lock().await.get_mut(&key) {
            known.retain(|pid, _| alive.contains(pid));
        }
        Some(alive.len())
    }
}

/// When `pid` started, as `ps` prints it, or `None` if there is no such
/// process or no `ps` to ask.
async fn started(pid: u32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    let out = tokio::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .await
        .ok()?;
    let started = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !started.is_empty()).then_some(started)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_running_process_reads_as_running_and_a_gone_one_does_not() {
        let live = Liveness::default();
        assert_eq!(live.running("work", Some("repo:main")).await, None);
        live.note("work", Some("repo:main"), std::process::id())
            .await;
        assert_eq!(live.running("work", Some("repo:main")).await, Some(1));

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        live.note("work", Some("repo:main"), child.id()).await;
        assert_eq!(live.running("work", Some("repo:main")).await, Some(2));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(live.running("work", Some("repo:main")).await, Some(1));
        // Another lane is its own.
        assert_eq!(live.running("work", None).await, None);
    }
}
