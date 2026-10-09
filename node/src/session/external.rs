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

/// When `pid` started, or `None` if there is no such process. Only compared
/// with itself, so each platform may say it its own way.
async fn started(pid: u32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    start_time(pid).await
}

/// Linux reads the start time from `/proc` rather than asking `ps`, which a
/// minimal image (a repository's check image, the node's container) may not
/// carry: without it every process would read as gone.
#[cfg(target_os = "linux")]
async fn start_time(pid: u32) -> Option<String> {
    let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
        .await
        .ok()?;
    stat_start_time(&stat).map(str::to_string)
}

/// Field 22 of `/proc/<pid>/stat`, `starttime`, in clock ticks since boot.
/// The command name (field 2) is in parentheses and may itself hold spaces
/// and parentheses, so fields are counted from after its last `)`.
#[cfg(any(target_os = "linux", test))]
fn stat_start_time(stat: &str) -> Option<&str> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // `rest` starts at field 3, the state.
    rest.split_whitespace().nth(22 - 3)
}

#[cfg(not(target_os = "linux"))]
async fn start_time(pid: u32) -> Option<String> {
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

    #[test]
    fn a_command_name_with_spaces_and_parentheses_does_not_shift_the_fields() {
        let stat = "4242 (a (b) c) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 \
                    12345 67 18446744073709551615";
        assert_eq!(stat_start_time(stat), Some("987654"));
        assert_eq!(stat_start_time("garbage"), None);
    }
}
