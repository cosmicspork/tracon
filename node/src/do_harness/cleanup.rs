//! What the Harness Runtime leaves for its caller to remove.
//!
//! - Agent configs: creating a session from a manifest stores one, and
//!   removing the session does not delete it. A config is leftover once no
//!   live session refers to it.
//! - Checkpoints: the runtime takes implicit ones as well as the ones asked
//!   for, each the size of the whole microVM (51 GB was recorded), and keeps
//!   them all until deleted. A prepared environment needs only its newest
//!   ready checkpoint to fork from.
//!
//! The decisions are plain functions over listings, so they are tested
//! without the API; the `prune_*` and `delete_*` functions list, decide and
//! delete. A failed deletion is reported and the rest carry on.

use std::collections::{BTreeMap, HashSet};

use super::{Checkpoint, CheckpointStatus, Client, ConfigSummary, DoError, Session, SessionFilter};
use crate::session::exhaustion::rfc3339_ms;

/// A config this young is spared even with no session on it: a session
/// create stores its config first, so a listing taken in between would show
/// the config without the session.
pub const CONFIG_MIN_AGE_MS: i64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigCleanup {
    pub deleted: Vec<ConfigSummary>,
    /// Config id and why its deletion failed.
    pub failed: Vec<(String, String)>,
}

/// The configs `owned` claims that no live session refers to and that are
/// at least `min_age_ms` old at `now_ms`. A config whose creation time cannot
/// be read is spared. `owned` keeps cleanup to configs the node made: the
/// same team may hold configs made by hand.
pub fn leftover_configs(
    configs: &[ConfigSummary],
    sessions: &[Session],
    owned: impl Fn(&ConfigSummary) -> bool,
    now_ms: i64,
    min_age_ms: i64,
) -> Vec<ConfigSummary> {
    let in_use: HashSet<&str> = sessions
        .iter()
        .filter(|s| !s.status.is_terminal())
        .filter_map(|s| s.config_id.as_deref())
        .collect();
    configs
        .iter()
        .filter(|c| owned(c))
        .filter(|c| !in_use.contains(c.id.as_str()))
        .filter(|c| rfc3339_ms(&c.created_at).is_some_and(|at| now_ms - at >= min_age_ms))
        .cloned()
        .collect()
}

/// Lists configs and sessions, then deletes the leftover configs.
pub async fn delete_leftover_configs(
    client: &Client,
    owned: impl Fn(&ConfigSummary) -> bool,
    now_ms: i64,
) -> Result<ConfigCleanup, DoError> {
    let configs = client.list_configs().await?;
    let sessions = client.list_sessions(&SessionFilter::default()).await?;
    let mut report = ConfigCleanup::default();
    for config in leftover_configs(&configs, &sessions, owned, now_ms, CONFIG_MIN_AGE_MS) {
        match client.delete_config(&config.id).await {
            Ok(()) => report.deleted.push(config),
            Err(error) => report.failed.push((config.id, error.to_string())),
        }
    }
    Ok(report)
}

/// What to do with one prepared environment's checkpoints.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Retention {
    pub keep: Vec<Checkpoint>,
    pub delete: Vec<Checkpoint>,
}

impl Retention {
    pub fn freed_bytes(&self) -> u64 {
        self.delete.iter().map(|c| c.size_bytes).sum()
    }
}

/// Keeps the newest READY checkpoint and any still PENDING (being taken);
/// deletes the older READY ones and the FAILED ones. A status this client
/// does not know is kept. `checkpoints` are one environment's, across all of
/// its sessions, in any order; where two share a creation time, the one
/// listed first counts as newer, which is the API's own order.
pub fn plan_checkpoint_retention(checkpoints: Vec<Checkpoint>) -> Retention {
    let mut ordered: Vec<(i64, usize, Checkpoint)> = checkpoints
        .into_iter()
        .enumerate()
        .map(|(i, c)| (rfc3339_ms(&c.created_at).unwrap_or(i64::MIN), i, c))
        .collect();
    ordered.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    let mut retention = Retention::default();
    let mut newest_ready_kept = false;
    for (_, _, checkpoint) in ordered {
        match checkpoint.status {
            CheckpointStatus::Ready if !newest_ready_kept => {
                newest_ready_kept = true;
                retention.keep.push(checkpoint);
            }
            CheckpointStatus::Ready | CheckpointStatus::Failed => retention.delete.push(checkpoint),
            CheckpointStatus::Pending | CheckpointStatus::Other(_) => {
                retention.keep.push(checkpoint)
            }
        }
    }
    retention
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckpointCleanup {
    pub kept: Vec<Checkpoint>,
    pub deleted: Vec<Checkpoint>,
    pub freed_bytes: u64,
    /// Checkpoint id and why its deletion failed.
    pub failed: Vec<(String, String)>,
}

/// Lists the checkpoints of every session `environment_of` assigns to a
/// prepared environment and keeps only each environment's newest ready one.
/// Sessions it returns `None` for are left alone, as is a session that has
/// gone between listing and asking.
pub async fn prune_checkpoints(
    client: &Client,
    sessions: &[Session],
    environment_of: impl Fn(&Session) -> Option<String>,
) -> Result<CheckpointCleanup, DoError> {
    let mut by_environment: BTreeMap<String, Vec<Checkpoint>> = BTreeMap::new();
    for session in sessions {
        let Some(environment) = environment_of(session) else {
            continue;
        };
        let checkpoints = match client.list_checkpoints(&session.session_id).await {
            Ok(checkpoints) => checkpoints,
            Err(error) if error.is_not_found() => continue,
            Err(error) => return Err(error),
        };
        by_environment
            .entry(environment)
            .or_default()
            .extend(checkpoints.into_iter().map(|mut c| {
                // The listing is per session, so the field may be left out.
                if c.session_id.is_empty() {
                    c.session_id = session.session_id.clone();
                }
                c
            }));
    }

    let mut report = CheckpointCleanup::default();
    for checkpoints in by_environment.into_values() {
        let retention = plan_checkpoint_retention(checkpoints);
        report.kept.extend(retention.keep);
        for checkpoint in retention.delete {
            match client
                .delete_checkpoint(&checkpoint.session_id, &checkpoint.checkpoint_id)
                .await
            {
                Ok(()) => {
                    report.freed_bytes += checkpoint.size_bytes;
                    report.deleted.push(checkpoint);
                }
                Err(error) => report
                    .failed
                    .push((checkpoint.checkpoint_id.clone(), error.to_string())),
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::super::SessionStatus;
    use super::*;

    const GB51: u64 = 51 * 1024 * 1024 * 1024;

    fn config(id: &str, name: &str, created_at: &str) -> ConfigSummary {
        ConfigSummary {
            id: id.into(),
            name: name.into(),
            created_at: created_at.into(),
            ..ConfigSummary::default()
        }
    }

    fn session(config_id: &str, status: SessionStatus) -> Session {
        Session {
            session_id: format!("s-{config_id}"),
            config_id: Some(config_id.into()),
            status,
            ..Session::default()
        }
    }

    fn checkpoint(id: &str, status: CheckpointStatus, created_at: &str) -> Checkpoint {
        Checkpoint {
            checkpoint_id: id.into(),
            session_id: "s".into(),
            status,
            size_bytes: GB51,
            created_at: created_at.into(),
            ..Checkpoint::default()
        }
    }

    #[test]
    fn a_config_is_leftover_only_when_owned_unused_and_old_enough() {
        let now = rfc3339_ms("2026-11-02T06:00:00Z").unwrap();
        let old = "2026-11-02T04:56:54.134365Z";
        let configs = vec![
            config("live", "tracon-a", old),
            config("paused", "tracon-b", old),
            config("destroyed", "tracon-c", old),
            config("orphan", "tracon-d", old),
            config("young", "tracon-e", "2026-11-02T05:58:00Z"),
            config("hand-made", "reviewer", old),
            config("no-time", "tracon-f", ""),
        ];
        let sessions = vec![
            session("live", SessionStatus::Ready),
            session("paused", SessionStatus::Paused),
            session("destroyed", SessionStatus::Destroyed),
        ];
        let leftover = leftover_configs(
            &configs,
            &sessions,
            |c| c.name.starts_with("tracon-"),
            now,
            CONFIG_MIN_AGE_MS,
        );
        let ids: Vec<&str> = leftover.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["destroyed", "orphan"]);
    }

    #[test]
    fn an_unknown_session_status_holds_its_config() {
        let now = rfc3339_ms("2026-11-02T06:00:00Z").unwrap();
        let configs = vec![config("c", "tracon-a", "2026-11-01T00:00:00Z")];
        let sessions = vec![session(
            "c",
            SessionStatus::Other("SESSION_STATUS_HIBERNATING".into()),
        )];
        assert!(leftover_configs(&configs, &sessions, |_| true, now, 0).is_empty());
    }

    #[test]
    fn retention_keeps_the_newest_ready_and_frees_the_rest() {
        let plan = plan_checkpoint_retention(vec![
            checkpoint("old", CheckpointStatus::Ready, "2026-11-01T10:00:00Z"),
            checkpoint("taking", CheckpointStatus::Pending, "2026-11-02T10:00:00Z"),
            checkpoint("broken", CheckpointStatus::Failed, "2026-11-02T09:00:00Z"),
            // Fractional seconds sort by value, not as text.
            checkpoint("newest", CheckpointStatus::Ready, "2026-11-02T08:00:00.5Z"),
            checkpoint("older", CheckpointStatus::Ready, "2026-11-02T08:00:00Z"),
        ]);
        let ids = |v: &[Checkpoint]| {
            v.iter()
                .map(|c| c.checkpoint_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&plan.keep), ["taking", "newest"]);
        assert_eq!(ids(&plan.delete), ["broken", "older", "old"]);
        assert_eq!(plan.freed_bytes(), 3 * GB51);
    }

    #[test]
    fn retention_with_nothing_ready_deletes_only_failures() {
        let plan = plan_checkpoint_retention(vec![
            checkpoint("p", CheckpointStatus::Pending, "2026-11-02T08:00:00Z"),
            checkpoint("f", CheckpointStatus::Failed, "2026-11-02T07:00:00Z"),
            checkpoint(
                "?",
                CheckpointStatus::Other("ARCHIVED".into()),
                "2026-11-02T06:00:00Z",
            ),
        ]);
        assert_eq!(plan.keep.len(), 2);
        assert_eq!(plan.delete.len(), 1);
        assert_eq!(plan.delete[0].checkpoint_id, "f");
    }
}
