//! Who the supervisor runs as, where its provider starts it as root.
//!
//! On Podman and Apple the container's own user is the sandbox's user, so
//! the supervisor, the harness, its services and every file it writes are
//! that user's. A provider that starts it under an init of its own, as root
//! (DigitalOcean's s6), names the user in the bootstrap instead
//! (`TRACON_SANDBOX_USER`). The supervisor reads what only root may (its
//! token) and then gives root up for good, before it starts a thread or a
//! process, so everything after is the same as on the other providers.
//!
//! This is consistency, not a boundary: the sandbox is the provider's VM,
//! whose root is its own, and a provider's isolation is read back from the
//! provider, never from inside. Root does not come back by another door
//! either: the supervisor sets no-new-privileges ([`no_new_privileges`]),
//! which everything it starts inherits, so a setuid or file-capability
//! binary in the image (`su`, `mount`, a `sudo` a snapshot carries) gains
//! nothing, as under Podman's `no-new-privileges`.

use std::io;

/// A numeric `uid:gid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

impl std::str::FromStr for User {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let number = |part: &str| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<u32>().ok())
                .flatten()
        };
        match text.split_once(':') {
            Some((uid, gid)) => match (number(uid), number(gid)) {
                (Some(uid), Some(gid)) => Ok(User { uid, gid }),
                _ => Err(format!("{text:?} is not a numeric uid:gid")),
            },
            None => Err(format!("{text:?} is not uid:gid")),
        }
    }
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// The user this process runs as now.
pub fn current() -> User {
    // SAFETY: these calls cannot fail.
    unsafe {
        User {
            uid: libc::geteuid(),
            gid: libc::getegid(),
        }
    }
}

/// Become `user` for good: no supplementary groups, real, effective and saved
/// ids all `user`'s. Already that user is nothing to do; any other user
/// than root cannot become another, which is an error rather than a sandbox
/// running as someone the node did not ask for. Call it before any thread
/// starts.
#[cfg(not(target_os = "linux"))]
pub fn become_user(user: User) -> io::Result<()> {
    match current() == user {
        true => no_new_privileges(),
        false => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("becoming {user} is supported on Linux only"),
        )),
    }
}

/// Become `user` for good: no supplementary groups, real, effective and saved
/// ids all `user`'s. Already that user is nothing to do; any other user
/// than root cannot become another, which is an error rather than a sandbox
/// running as someone the node did not ask for. Call it before any thread
/// starts.
#[cfg(target_os = "linux")]
pub fn become_user(user: User) -> io::Result<()> {
    let ids = || unsafe {
        let (mut r, mut e, mut s) = (0, 0, 0);
        let (mut rg, mut eg, mut sg) = (0, 0, 0);
        libc::getresuid(&mut r, &mut e, &mut s);
        libc::getresgid(&mut rg, &mut eg, &mut sg);
        ([r, e, s], [rg, eg, sg])
    };
    let is = |(uids, gids): ([u32; 3], [u32; 3])| {
        uids.iter().all(|&u| u == user.uid) && gids.iter().all(|&g| g == user.gid)
    };
    if is(ids()) {
        return no_new_privileges();
    }
    let now = current();
    if now.uid != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("running as {now}, which cannot become {user}"),
        ));
    }
    // SAFETY: plain system calls on this process's credentials; the group
    // goes first, while the process may still change it.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0
            || libc::setresgid(user.gid, user.gid, user.gid) != 0
            || libc::setresuid(user.uid, user.uid, user.uid) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if !is(ids()) {
        return Err(io::Error::other(format!(
            "asked to become {user}, and is {} instead",
            current()
        )));
    }
    // Root is gone for good only if it cannot be taken back.
    if user.uid != 0 && unsafe { libc::setuid(0) } == 0 {
        return Err(io::Error::other(format!(
            "became {user}, and could still become root again"
        )));
    }
    no_new_privileges()
}

/// Set no-new-privileges on this process, which every process it starts
/// inherits and none can clear: an exec of a setuid or file-capability
/// binary then runs with the caller's own credentials. Failing to set it is
/// an error, never a sandbox running without it.
#[cfg(target_os = "linux")]
pub fn no_new_privileges() -> io::Result<()> {
    // SAFETY: a plain prctl on this process.
    match unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } {
        0 => Ok(()),
        _ => Err(io::Error::other(format!(
            "set no-new-privileges: {}",
            io::Error::last_os_error()
        ))),
    }
}

/// Elsewhere there is nothing to set.
#[cfg(not(target_os = "linux"))]
pub fn no_new_privileges() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_is_numeric_uid_and_gid() {
        assert_eq!(
            "65532:65532".parse::<User>().unwrap(),
            User {
                uid: 65532,
                gid: 65532
            }
        );
        assert_eq!("0:0".parse::<User>().unwrap().to_string(), "0:0");
        for bad in [
            "",
            "65532",
            "harness:harness",
            "1:",
            ":1",
            "-1:1",
            "1:2:3",
            "+1:1",
        ] {
            assert!(bad.parse::<User>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn becoming_who_it_is_already_is_nothing_to_do() {
        become_user(current()).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_user_other_than_root_cannot_become_another() {
        let now = current();
        if now.uid == 0 {
            return;
        }
        let other = User {
            uid: now.uid + 1,
            gid: now.gid,
        };
        let error = become_user(other).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("cannot become"), "{error}");
        assert_eq!(current(), now);
    }
}
