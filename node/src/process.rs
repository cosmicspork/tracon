//! Process identity: what a pid names, beyond the number.
//!
//! A pid is not an identity. The kernel hands the number back out after the
//! process that held it ends, so a record left behind by a node that died —
//! the desktop app's handoff record, a pid file — can name a process that has
//! nothing to do with this one. Adopting it blocks setup behind a stranger;
//! signalling it sends SIGTERM, then SIGKILL, to that stranger.
//!
//! So a record has to carry something the kernel does not reuse: the start
//! time, which is fixed for the life of a process and only ever equal to
//! itself, and the executable the process is running. Both are read from the
//! operating system at the moment of verification; what a record says is only
//! ever compared against them, never believed. A record that cannot be checked
//! — one written before start times were recorded — is refused for the same
//! reason, because nothing in it distinguishes the node it named from whatever
//! holds that pid now. Refusal is loud and removes the record.
//!
//! This is the tested copy. The desktop app mirrors the same logic in
//! `wrapper/src/node.rs`, because it reads its own handoff record at launch,
//! before it has installed the CLI that carries this crate; the two are meant
//! to stay in agreement.
//!
//! Unrelated to `crate::transfers`, whose "handoff" is a continuity package
//! moving between nodes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a process is, beyond its pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub pid: u32,
    /// The start time as the platform reports it, kept as the opaque string it
    /// arrived as: it is only ever compared for equality, and parsing it would
    /// only invent a way to be wrong about it.
    pub started: String,
    /// The executable the process is running, when the platform will say.
    pub exe: Option<String>,
}

impl Identity {
    /// The process with this pid, as it is now. `None` when there is none.
    pub fn of(pid: u32) -> Option<Self> {
        if pid == 0 {
            return None;
        }
        Some(Self {
            pid,
            started: started(pid)?,
            exe: exe(pid),
        })
    }

    pub fn of_self() -> Option<Self> {
        Self::of(std::process::id())
    }
}

/// Why a record no longer names the process it was written for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Stale {
    #[error("the record names no process")]
    NoPid,
    #[error("the record carries no start time, so nothing ties it to a running process")]
    Unverifiable,
    #[error("no process {pid} is running")]
    Gone { pid: u32 },
    #[error("pid {pid} was reused: it started at {found}, the record at {recorded}")]
    Reused {
        pid: u32,
        recorded: String,
        found: String,
    },
    #[error("pid {pid} is running {}, not {recorded}", .found.as_deref().unwrap_or("something this process cannot see"))]
    OtherBinary {
        pid: u32,
        recorded: String,
        found: Option<String>,
    },
}

/// A process written down for something else to find later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: u32,
    /// Absent in records written before start times were recorded, and in
    /// anything else that only knew the number. Such a record is refused.
    #[serde(default)]
    pub started: Option<String>,
    #[serde(default)]
    pub exe: Option<String>,
}

impl From<Identity> for Record {
    fn from(id: Identity) -> Self {
        Self {
            pid: id.pid,
            started: Some(id.started),
            exe: id.exe,
        }
    }
}

impl Record {
    /// A record naming this process.
    pub fn of_self() -> Option<Self> {
        Identity::of_self().map(Self::from)
    }

    /// The pid, if the process holding it now is the one this record was
    /// written for. Reads the system itself.
    pub fn verify(&self) -> Result<u32, Stale> {
        self.verify_against(Identity::of(self.pid).as_ref())
    }

    /// `verify` against an identity handed in, so the comparison is testable
    /// without a process that has the properties under test.
    pub fn verify_against(&self, now: Option<&Identity>) -> Result<u32, Stale> {
        if self.pid == 0 {
            return Err(Stale::NoPid);
        }
        let Some(recorded) = self.started.as_deref() else {
            return Err(Stale::Unverifiable);
        };
        let Some(now) = now.filter(|n| n.pid == self.pid) else {
            return Err(Stale::Gone { pid: self.pid });
        };
        if now.started != recorded {
            return Err(Stale::Reused {
                pid: self.pid,
                recorded: recorded.to_string(),
                found: now.started.clone(),
            });
        }
        // A recorded executable the running process cannot be shown to share
        // is a refusal, including when this process may not look: an answer
        // that is not "the same binary" is not an answer.
        if let Some(recorded_exe) = self.exe.as_deref() {
            if now.exe.as_deref() != Some(recorded_exe) {
                return Err(Stale::OtherBinary {
                    pid: self.pid,
                    recorded: recorded_exe.to_string(),
                    found: now.exe.clone(),
                });
            }
        }
        Ok(self.pid)
    }
}

/// The record a desktop app leaves beside the node's state for a node it
/// spawned itself.
pub fn desktop_handoff_path(state_dir: &Path) -> PathBuf {
    state_dir.join("desktop-node.json")
}

/// The pid of a spawned node still worth adopting or signalling. A record that
/// no longer names its process — gone, reused, or too old to carry a start
/// time — is said out loud and removed, because the alternative is adopting or
/// killing whatever holds that number now.
pub fn adopt_desktop_handoff(state_dir: &Path) -> Option<u32> {
    let path = desktop_handoff_path(state_dir);
    let bytes = std::fs::read(&path).ok()?;
    let record: Record = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "desktop handoff record is unreadable; removed");
            let _ = std::fs::remove_file(&path);
            return None;
        }
    };
    match record.verify() {
        Ok(pid) => Some(pid),
        Err(why) => {
            tracing::warn!(
                path = %path.display(),
                pid = record.pid,
                reason = %why,
                "desktop handoff record no longer names its process; refused and removed"
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Write the handoff record for a node being spawned. The start time is what
/// makes it verifiable later, so the record is only ever written from one.
pub fn write_desktop_handoff(state_dir: &Path, record: &Record) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let json = serde_json::to_vec(record)?;
    std::fs::write(desktop_handoff_path(state_dir), json)
}

pub fn clear_desktop_handoff(state_dir: &Path) {
    let _ = std::fs::remove_file(desktop_handoff_path(state_dir));
}

/// The start time of a process, as an opaque token.
///
/// Field 22 of `/proc/<pid>/stat` is the start time in clock ticks since boot.
/// Field 2 is the executable name in parentheses and may contain spaces and
/// parentheses of its own, so the fields are counted from the last `)`.
#[cfg(target_os = "linux")]
fn started(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(19).map(str::to_string)
}

/// macOS has no `/proc`. `ps` reports the start time to the second, and
/// shelling out beats a libproc binding and an unsafe block for one field —
/// the same trade `service::uid` makes.
#[cfg(not(target_os = "linux"))]
fn started(pid: u32) -> Option<String> {
    ps_field(pid, "lstart=")
}

#[cfg(target_os = "linux")]
fn exe(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn exe(pid: u32) -> Option<String> {
    ps_field(pid, "comm=")
}

#[cfg(not(target_os = "linux"))]
fn ps_field(pid: u32, field: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child that ends when the test is done with it.
    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("spawning sleep")
    }

    #[test]
    fn a_running_process_has_a_start_time_and_a_pid_that_is_free_has_none() {
        let me = Identity::of_self().expect("this process is running");
        assert_eq!(me.pid, std::process::id());
        assert!(!me.started.is_empty());
        // Larger than any pid a kernel hands out, so it names nothing.
        assert_eq!(Identity::of(u32::MAX), None);
        assert_eq!(Identity::of(0), None);
    }

    #[test]
    fn a_record_holds_while_its_process_runs_and_is_stale_once_it_exits() {
        let mut child = sleeper();
        let pid = child.id();
        let record = Record::from(Identity::of(pid).expect("the child is running"));
        assert_eq!(record.verify(), Ok(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(record.verify(), Err(Stale::Gone { pid }));
    }

    #[test]
    fn a_reused_pid_is_refused_rather_than_adopted() {
        // What pid reuse looks like from here: the number is live, but the
        // process holding it is not the one that was written down.
        let me = Identity::of_self().unwrap();
        let record = Record {
            pid: me.pid,
            started: Some(format!("{}0", me.started)),
            exe: None,
        };
        assert!(matches!(record.verify(), Err(Stale::Reused { pid, .. }) if pid == me.pid));
    }

    #[test]
    fn a_record_without_a_start_time_is_refused_even_while_its_pid_is_live() {
        let record = Record {
            pid: std::process::id(),
            started: None,
            exe: None,
        };
        assert_eq!(record.verify(), Err(Stale::Unverifiable));
        assert_eq!(
            Record {
                pid: 0,
                started: None,
                exe: None
            }
            .verify(),
            Err(Stale::NoPid)
        );
    }

    #[test]
    fn a_pid_running_another_binary_is_refused() {
        let me = Identity::of_self().unwrap();
        let record = Record {
            exe: Some("/nowhere/tracon".into()),
            ..Record::from(me.clone())
        };
        assert!(matches!(record.verify(), Err(Stale::OtherBinary { .. })));
        // And when the platform will not say what it is running, which is not
        // the same answer as "the same binary".
        let hidden = Identity { exe: None, ..me };
        let record = Record {
            exe: Some("/nowhere/tracon".into()),
            ..Record::from(hidden.clone())
        };
        assert!(matches!(
            record.verify_against(Some(&hidden)),
            Err(Stale::OtherBinary { found: None, .. })
        ));
    }

    #[test]
    fn a_record_naming_a_different_pid_than_the_process_found_is_stale() {
        let me = Identity::of_self().unwrap();
        let record = Record {
            pid: me.pid + 1,
            ..Record::from(me.clone())
        };
        assert_eq!(
            record.verify_against(Some(&me)),
            Err(Stale::Gone { pid: me.pid + 1 })
        );
    }

    #[test]
    fn a_live_handoff_record_is_adopted_and_a_stale_one_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(adopt_desktop_handoff(dir.path()), None);

        let mut child = sleeper();
        let pid = child.id();
        let record = Record::from(Identity::of(pid).unwrap());
        write_desktop_handoff(dir.path(), &record).unwrap();
        assert_eq!(adopt_desktop_handoff(dir.path()), Some(pid));
        assert!(desktop_handoff_path(dir.path()).exists());

        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(adopt_desktop_handoff(dir.path()), None);
        assert!(
            !desktop_handoff_path(dir.path()).exists(),
            "a record whose process is gone is removed"
        );
    }

    #[test]
    fn a_handoff_record_that_predates_start_times_is_refused_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = sleeper();
        let pid = child.id();
        // What the desktop app used to write: the number and nothing else.
        std::fs::write(
            desktop_handoff_path(dir.path()),
            format!(r#"{{"pid":{pid},"binary":"/opt/tracon"}}"#),
        )
        .unwrap();
        assert_eq!(adopt_desktop_handoff(dir.path()), None);
        assert!(!desktop_handoff_path(dir.path()).exists());
        // And the process it named is left alone: refusing is not killing.
        assert!(Identity::of(pid).is_some());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn an_unreadable_handoff_record_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(desktop_handoff_path(dir.path()), b"{not json").unwrap();
        assert_eq!(adopt_desktop_handoff(dir.path()), None);
        assert!(!desktop_handoff_path(dir.path()).exists());
        clear_desktop_handoff(dir.path());
    }
}
